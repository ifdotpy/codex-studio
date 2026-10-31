use serde_json::{Value, json};
use std::sync::Mutex;
use studio_runtime_services::account_session_runtime::{
    AccountSessionError, AccountSessionFuture, AccountSessionPort, LimitsSnapshot, LoginRequest,
    limits,
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
fn profile_options_preserve_aliases_and_reject_invalid_shapes() {
    let home = tempfile::tempdir().expect("temporary home");
    let normalize = |value: Value| {
        studio_runtime_services::account_session_runtime::normalize_profile(
            value.as_object().expect("options object").clone(),
            home.path(),
        )
    };
    let result = normalize(serde_json::json!({"customModels":["model", {"id":"other","label":null}], "autoCompactWindow":"100000"})).expect("aliases normalize");
    assert_eq!(
        result["customModels"],
        serde_json::json!([{"id":"model","label":"model"},{"id":"other","label":"other"}])
    );
    assert_eq!(result["autoCompactWindow"], serde_json::json!(100000));
    for value in [
        serde_json::json!({"binaryPath":null}),
        serde_json::json!({"configDir":null}),
        serde_json::json!({"customModels":null}),
        serde_json::json!({"launchArgs":null}),
        serde_json::json!({"customModels":[["model","Model"]]}),
        serde_json::json!({"customModels":[{"id":"model","label":"Model","extra":true}]}),
        serde_json::json!({"autoCompactWindow":100000.0}),
        serde_json::json!({"autoCompactWindow":"99999"}),
        serde_json::json!({"unknown":true}),
    ] {
        assert!(normalize(value.clone()).is_err(), "{value}");
    }
    assert!(normalize(serde_json::json!({"autoCompactWindow":null})).is_ok());
}
