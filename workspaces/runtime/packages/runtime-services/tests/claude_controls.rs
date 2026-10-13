#[path = "support/prepared_python.rs"]
mod python_test_support;
use serde_json::Value;
use studio_runtime_services::claude_controls::{ControlRequest, ControlSnapshot, settings};
#[test]
fn python_control_transactions_and_settings() {
    let output = std::process::Command::new(python_test_support::prepared_python(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    ))
    .args([
        "-B",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/claude_runtime/controls_oracle.py"
        ),
    ])
    .output()
    .expect("valid fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let corpus: Value = serde_json::from_slice(&output.stdout).expect("valid fixture");
    for case in corpus["cases"].as_array().expect("valid fixture") {
        let request = ControlRequest::from_value(&case["body"]).expect("valid fixture");
        let snapshot =
            ControlSnapshot::from_records(case["agent"].clone(), None).expect("valid fixture");
        let mut writes = snapshot
            .reserve(&request, false, 120.0)
            .expect("valid fixture");
        assert_eq!(
            writes.operation, case["stages"][0]["operation"],
            "{} reserve",
            case["name"]
        );
        let mut snapshot =
            ControlSnapshot::from_records(writes.agent.clone(), Some(writes.operation.clone()))
                .expect("valid fixture");
        if !case["agent"]["threadId"].is_null() {
            let submitted = snapshot
                .submitted(&request, &case["native"], &case["queue"], 120.0)
                .expect("valid fixture");
            assert_eq!(
                submitted, case["stages"][1]["operation"],
                "{} submit",
                case["name"]
            );
            snapshot = ControlSnapshot::from_records(writes.agent.clone(), Some(submitted))
                .expect("valid fixture");
            assert!(matches!(
                snapshot.reserve(&request, false, 121.0),
                Err(studio_runtime_services::claude_controls::ControlError::Unknown)
            ));
        }
        let provider = snapshot
            .provider_ready(case["provider"].clone(), 120.0)
            .expect("valid fixture");
        snapshot = ControlSnapshot::from_records(writes.agent.clone(), Some(provider))
            .expect("valid fixture");
        writes = snapshot.complete(&request, 120.0).expect("valid fixture");
        assert_eq!(
            writes.agent, case["expected"]["agent"],
            "{} agent",
            case["name"]
        );
        assert_eq!(
            writes.operation, case["expected"]["operation"],
            "{} operation",
            case["name"]
        );
        assert_eq!(writes.result, case["expected"]["result"]);
        let done = ControlSnapshot::from_records(writes.agent, Some(writes.operation))
            .expect("valid fixture");
        assert_eq!(
            done.reserve(&request, false, 121.0)
                .expect("valid fixture")
                .result,
            writes.result
        );
    }
    for row in corpus["settings"].as_array().expect("valid fixture") {
        let result = settings(&row["input"]);
        assert_eq!(
            result.is_err(),
            row["error"].as_bool().expect("valid fixture")
        );
        if let Ok(result) = result {
            assert_eq!(result, row["expected"]);
        }
    }
}
