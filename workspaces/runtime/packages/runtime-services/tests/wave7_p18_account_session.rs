#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;

use serde_json::{Value, json};
use std::{path::Path, process::Command, sync::Mutex};
use studio_runtime_services::account_session_runtime::{
    AccountSessionError, AccountSessionPort, LimitsSnapshot, LoginRequest, limits,
};

struct Session {
    record: Value,
    fresh: Value,
    calls: Mutex<Vec<Value>>,
}
impl AccountSessionPort for Session {
    fn limits_snapshot(&self, _: &str) -> Result<Value, AccountSessionError> {
        Ok(self.record.clone())
    }
    fn refresh_limits(&self, account: &str) -> Result<(), AccountSessionError> {
        self.calls
            .lock()
            .expect("calls")
            .push(json!(["refresh", account]));
        Ok(())
    }
    fn read_limits(&self, account: &str) -> Result<Value, AccountSessionError> {
        self.calls
            .lock()
            .expect("calls")
            .push(json!(["read", account]));
        Ok(self.fresh.clone())
    }
}

#[test]
fn limits_match_real_python_route_for_missing_null_empty_and_present_fields() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let python = prepared_python_support::prepared_python(&root.join("../../apps/server/src"));
    let output = Command::new(python)
        .arg(root.join("tests/wave7_p18_limits_oracle.py"))
        .output()
        .expect("Python oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("Python-written corpus");
    assert_eq!(cases.len(), 32);
    for (index, case) in cases.into_iter().enumerate() {
        let session = Session {
            record: case["record"].clone(),
            fresh: case["fresh"].clone(),
            calls: Mutex::default(),
        };
        let response = limits(
            &session,
            "fixture",
            case["cached"].as_bool().expect("cached"),
        )
        .expect("valid limits response");
        assert_eq!(response, case["result"], "case {index}");
        assert_eq!(
            json!(*session.calls.lock().expect("calls")),
            case["calls"],
            "case {index}"
        );
    }
}

#[test]
fn malformed_and_foreign_snapshots_refuse_before_effects() {
    for record in [
        json!(null),
        json!({"accountKey":"other"}),
        json!({"accountKey":"fixture","error":false}),
        json!({"accountKey":"fixture","data":[]} ),
    ] {
        let session = Session {
            record,
            fresh: json!({}),
            calls: Mutex::default(),
        };
        assert!(limits(&session, "fixture", false).is_err());
        assert!(session.calls.lock().expect("calls").is_empty());
    }
    assert!(LimitsSnapshot::read("fixture", json!({"accountKey":"fixture","data":null})).is_ok());
}

#[test]
fn absent_consumers_are_named_refusals() {
    struct Absent;
    impl AccountSessionPort for Absent {}
    let request = LoginRequest {
        login_id: "fixture-request".into(),
        account_key: None,
        email: None,
        label: None,
    };
    assert!(matches!(
        Absent.login(request),
        Err(AccountSessionError::Requires(
            "provider-session/codex-login"
        ))
    ));
    assert!(matches!(
        Absent.cancel_login("fixture-request"),
        Err(AccountSessionError::Requires(
            "provider-session/codex-login"
        ))
    ));
    assert!(matches!(
        limits(&Absent, "fixture", false),
        Err(AccountSessionError::Requires(
            "provider-session/account-rate-limits"
        ))
    ));
}

#[test]
fn profile_normalization_matches_python() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(prepared_python_support::prepared_python(
        &root.join("../../apps/server/src"),
    ))
    .arg(root.join("tests/wave7_p18_profile_oracle.py"))
    .output()
    .expect("oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("corpus");
    assert_eq!(cases.len(), 27);
    for case in cases {
        let result = studio_runtime_services::account_session_runtime::normalize_profile(
            case["options"].as_object().expect("options").clone(),
            Path::new(case["home"].as_str().expect("home")),
        );
        let normalized = result
            .as_ref()
            .ok()
            .cloned()
            .map(Value::Object)
            .unwrap_or(Value::Null);
        assert_eq!(
            normalized,
            case["result"],
            "{}; normalization error: {:?}",
            case["options"],
            result.err()
        );
    }
}
