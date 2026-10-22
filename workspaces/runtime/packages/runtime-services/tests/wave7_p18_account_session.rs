use serde_json::{Value, json};
use std::{path::Path, sync::Mutex};
use studio_runtime_services::account_session_runtime::{
    AccountSessionError, AccountSessionFuture, AccountSessionPort, LimitsSnapshot, LoginRequest,
    limits,
};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsCase {
    record: Value,
    fresh: Value,
    cached: bool,
    result: Value,
    calls: Vec<[String; 2]>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileCase {
    options: serde_json::Map<String, Value>,
    home: String,
    result: Value,
}

struct Session {
    record: Value,
    fresh: Value,
    calls: Mutex<Vec<Value>>,
}
impl AccountSessionPort for Session {
    fn limits_snapshot(&self, _: &str) -> Result<Value, AccountSessionError> {
        Ok(self.record.clone())
    }
    fn refresh_limits<'a>(&'a self, account: &'a str) -> AccountSessionFuture<'a, ()> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("calls")
                .push(json!(["refresh", account]));
            Ok(())
        })
    }
    fn read_limits<'a>(&'a self, account: &'a str) -> AccountSessionFuture<'a, Value> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("calls")
                .push(json!(["read", account]));
            Ok(self.fresh.clone())
        })
    }
}

#[tokio::test]
async fn limits_match_real_python_route_for_missing_null_empty_and_present_fields() {
    let cases: Vec<LimitsCase> = serde_json::from_str(include_str!("wave7_p18_limits_corpus.json"))
        .expect("frozen limits corpus");
    assert_eq!(cases.len(), 32);
    for (index, case) in cases.into_iter().enumerate() {
        let session = Session {
            record: case.record,
            fresh: case.fresh,
            calls: Mutex::default(),
        };
        let response = limits(&session, "fixture", case.cached)
            .await
            .expect("valid limits response");
        assert_eq!(response, case.result, "case {index}");
        assert_eq!(
            *session.calls.lock().expect("calls"),
            case.calls
                .iter()
                .map(|call| json!(call))
                .collect::<Vec<_>>(),
            "case {index}"
        );
    }
}

#[tokio::test]
async fn malformed_and_foreign_snapshots_refuse_before_effects() {
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
        assert!(limits(&session, "fixture", false).await.is_err());
        assert!(session.calls.lock().expect("calls").is_empty());
    }
    assert!(LimitsSnapshot::read("fixture", json!({"accountKey":"fixture","data":null})).is_ok());
}

#[tokio::test]
async fn absent_consumers_are_named_refusals() {
    struct Absent;
    impl AccountSessionPort for Absent {}
    let request = LoginRequest {
        login_id: "fixture-request".into(),
        account_key: None,
        email: None,
        label: None,
    };
    assert!(matches!(
        Absent.login(request).await,
        Err(AccountSessionError::Requires(
            "provider-session/codex-login"
        ))
    ));
    assert!(matches!(
        Absent.cancel_login("fixture-request").await,
        Err(AccountSessionError::Requires(
            "provider-session/codex-login"
        ))
    ));
    assert!(matches!(
        Absent.claude_control_session("fixture-account"),
        Err(AccountSessionError::Requires(
            "provider-session/claude-controls"
        ))
    ));
    assert!(matches!(
        limits(&Absent, "fixture", false).await,
        Err(AccountSessionError::Requires(
            "provider-session/account-rate-limits"
        ))
    ));
}

#[test]
fn profile_normalization_matches_python() {
    let cases: Vec<ProfileCase> =
        serde_json::from_str(include_str!("wave7_p18_profile_corpus.json"))
            .expect("frozen profile corpus");
    assert_eq!(cases.len(), 27);
    for case in cases {
        let result = studio_runtime_services::account_session_runtime::normalize_profile(
            case.options.clone(),
            Path::new(&case.home),
        );
        let normalized = result
            .as_ref()
            .ok()
            .cloned()
            .map(Value::Object)
            .unwrap_or(Value::Null);
        assert_eq!(
            normalized,
            case.result,
            "{:?}; normalization error: {:?}",
            case.options,
            result.err()
        );
    }
}
