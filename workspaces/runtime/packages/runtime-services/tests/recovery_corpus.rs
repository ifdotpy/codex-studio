#[path = "support/prepared_python.rs"]
mod python_test_support;
use serde_json::Value;
use std::{path::Path, process::Command};
use studio_runtime_services::recovery_policy::{RecoverySnapshot, native_operations_settled};
#[test]
fn python_written_eligibility_corpus() {
    let python = python_test_support::prepared_python(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    );
    let result = Command::new(python)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/recovery/oracle.py"))
        .output()
        .expect("oracle starts");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output: Value = serde_json::from_slice(&result.stdout).expect("oracle JSON");
    let cases = output["eligibility"]
        .as_array()
        .expect("valid recovery fixture");
    assert!(cases.len() > 300);
    for operation in output["operations"]
        .as_array()
        .expect("valid recovery fixture")
    {
        assert_eq!(
            native_operations_settled(&operation["turn"]),
            operation["expected"]
                .as_bool()
                .expect("valid recovery fixture"),
            "{}",
            operation["turn"]
        );
    }
    for case in output["completion"]
        .as_array()
        .expect("valid recovery fixture")
    {
        let snapshot =
            RecoverySnapshot::from_record(case["agent"].clone()).expect("valid recovery fixture");
        assert_eq!(
            studio_runtime_services::recovery::completion_permission(
                &snapshot,
                &case["turn"],
                case["unresolved"]
                    .as_bool()
                    .expect("valid recovery fixture")
            ),
            case["expected"].as_bool().expect("valid recovery fixture"),
            "{}",
            case["name"]
        );
    }
    for case in cases {
        let snapshot = RecoverySnapshot::from_record(case["agent"].clone())
            .unwrap_or_else(|e| panic!("{}: {e}", case["name"]));
        assert_eq!(
            serde_json::to_value(snapshot.eligibility()).expect("valid recovery fixture"),
            case["expected"],
            "{}",
            case["name"]
        );
    }
}

#[test]
fn malformed_native_items_never_authorize_completion() {
    for items in [
        serde_json::json!(null),
        serde_json::json!("invalid"),
        serde_json::json!({}),
    ] {
        assert!(!native_operations_settled(
            &serde_json::json!({"items":items})
        ));
    }
}
