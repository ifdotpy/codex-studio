use serde_json::Value;
use studio_runtime_services::recovery_policy::{RecoverySnapshot, native_operations_settled};
#[test]
fn python_written_eligibility_corpus() {
    let output: Value = serde_json::from_str(include_str!("recovery/oracle.json"))
        .expect("frozen eligibility corpus");
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

#[test]
fn recovery_record_typed_boundary_matches_schema() {
    let base = serde_json::json!({"id":"a","rootId":"a","epoch":1,"status":"running"});
    for (field, values) in [
        (
            "epoch",
            vec![
                serde_json::json!(1.0),
                serde_json::json!(-1),
                serde_json::json!(9007199254740992_u64),
            ],
        ),
        (
            "events",
            vec![
                serde_json::json!(null),
                serde_json::json!(1.0),
                serde_json::json!(-1),
            ],
        ),
        (
            "startAttempt",
            vec![
                serde_json::json!([]),
                serde_json::json!({"events":[1]}),
                serde_json::json!({"epoch":1.0}),
            ],
        ),
        ("id", vec![serde_json::json!(""), serde_json::json!(null)]),
    ] {
        for value in values {
            let mut candidate = base.clone();
            candidate[field] = value;
            let expected =
                studio_runtime_schema::validate("RecoveryAgentRecord", &candidate).expect("schema");
            let actual = serde_json::from_value::<
                studio_runtime_schema::recovery_record::RecoveryAgentRecord,
            >(candidate.clone())
            .is_ok();
            assert_eq!(actual, expected, "{candidate}");
            let snapshot = RecoverySnapshot::from_record(candidate.clone());
            assert_eq!(snapshot.is_ok(), expected, "snapshot: {candidate}");
            if let Ok(snapshot) = snapshot {
                assert_eq!(snapshot.record(), &candidate);
            }
        }
    }
    assert!(
        serde_json::from_value::<studio_runtime_schema::recovery_record::RecoveryAgentRecord>(
            serde_json::json!(["a", "a", 1, "running"])
        )
        .is_err()
    );
}

#[test]
fn snapshot_rejects_object_encoded_request_state() {
    let mut record = serde_json::json!({"id":"a","rootId":"a","epoch":1,"status":"running",
        "connectionRecovery":{"requestState":"pending"}});
    assert!(RecoverySnapshot::from_record(record.clone()).is_ok());
    record["connectionRecovery"]["requestState"] = serde_json::json!({"pending":null});
    assert!(RecoverySnapshot::from_record(record).is_err());
}
