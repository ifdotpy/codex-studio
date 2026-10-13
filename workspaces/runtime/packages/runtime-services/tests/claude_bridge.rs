use serde_json::{Value, json};
use studio_runtime_services::{
    account_driver::{AuthMetadata, AuthStatus},
    claude_bridge::{BridgeSnapshot, ResolvedBridge, admission_hold},
};
#[test]
fn python_launch_plans_and_explicit_recovery_holds() {
    let output =
        std::process::Command::new(std::env::var_os("CODEX_AGENTS_PYTHON").expect("valid fixture"))
            .args([
                "-B",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/claude_runtime/bridge_oracle.py"
                ),
            ])
            .output()
            .expect("valid fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let corpus: Vec<Value> = serde_json::from_slice(&output.stdout).expect("valid fixture");
    for case in corpus {
        let snapshot = BridgeSnapshot::from_record(case["profile"].clone()).expect("valid fixture");
        let auth = AuthMetadata {
            status: AuthStatus::Ready,
            account_id: Some("claude:fixture@example.test".into()),
            email: Some("fixture@example.test".into()),
            plan: Some("max".into()),
            credential_identity: None,
            error: None,
            error_kind: None,
        };
        let launch = snapshot.launch(
            ResolvedBridge {
                node: "/fixture/node".into(),
                executable: "/fixture/cli".into(),
                bridge: "/fixture/bridge/bridge.mjs".into(),
                root: "/fixture/root".into(),
                sdk_present: true,
            },
            &auth,
            std::collections::BTreeMap::from([
                ("HOME".into(), "/fixture/home".into()),
                ("PATH".into(), "/fixture/path".into()),
                ("ANTHROPIC_API_KEY".into(), "fixture-value".into()),
            ]),
        );
        if case["expected"].is_null() {
            assert!(launch.is_err());
            continue;
        }
        let launch = launch.expect("valid fixture");
        let mut actual = json!({"command":launch.command,"environment":launch.environment});
        actual["environment"]["STUDIO_CLAUDE_OPTIONS"] = serde_json::from_str(
            actual["environment"]["STUDIO_CLAUDE_OPTIONS"]
                .as_str()
                .expect("valid fixture"),
        )
        .expect("valid fixture");
        assert_eq!(actual, case["expected"]);
    }
    for key in ["claudePreInputRetry", "claudeAuthWait"] {
        assert!(admission_hold(&json!({key:{"original":"preserved"}})).is_some());
    }
    assert!(
        admission_hold(
            &json!({"provider":"claude","startAttempt":{"executionOutcome":"not_applied"}})
        )
        .is_some()
    );
}
