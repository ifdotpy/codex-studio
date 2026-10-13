use serde_json::Value;
use studio_runtime_services::claude_login::{LoginSnapshot, validate_code, verification_url};
#[test]
fn python_receipt_writer_code_once_and_validation() {
    let python = std::env::var_os("CODEX_AGENTS_PYTHON").expect("isolated interpreter");
    let output = std::process::Command::new(python)
        .arg("-B")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/claude_runtime/login_oracle.py"
        ))
        .output()
        .expect("valid fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let corpus: Value = serde_json::from_slice(&output.stdout).expect("valid fixture");
    for row in corpus["rows"].as_array().expect("valid fixture") {
        let snapshot = LoginSnapshot::from_record(row["input"].clone()).expect("valid fixture");
        let result = snapshot.code_intent(row["ready"].as_bool().expect("valid fixture"));
        if !row["error"].is_null() {
            assert!(result.is_err(), "{}", row["name"]);
            continue;
        }
        let (next, write) = result.expect("valid fixture");
        assert_eq!(
            write as u64,
            row["writes"].as_u64().expect("valid fixture"),
            "{}",
            row["name"]
        );
        assert_eq!(next.record(), &row["expected"], "{}", row["name"]);
    }
    for (hint, email, accepted) in [
        ("straße@example.test", "straße@example.test", true),
        ("Fixture@example.test", "fixture@EXAMPLE.TEST", true),
        ("Straße@example.test", "STRASSE@EXAMPLE.TEST", false),
        ("Σ@example.test", "ς@example.test", false),
        ("ﬃ@example.test", "ffi@example.test", false),
        ("wrong@example.test", "fixture@example.test", false),
    ] {
        let mut record = corpus["rows"][0]["input"].clone();
        record["metadata"]["flow"] = serde_json::json!("add");
        record["metadata"]["emailHint"] = serde_json::json!(hint);
        let snapshot =
            LoginSnapshot::from_record(record).expect("Python receipt with add metadata");
        let metadata = studio_runtime_services::account_driver::AuthMetadata {
            status: studio_runtime_services::account_driver::AuthStatus::Ready,
            account_id: Some(format!("claude:{email}")),
            email: Some(email.into()),
            plan: Some("max".into()),
            credential_identity: None,
            error: None,
            error_kind: None,
        };
        assert_eq!(
            snapshot.validate_completion(&metadata, true).is_ok(),
            accepted
        );
    }
    for row in corpus["chats"].as_array().expect("chat corpus") {
        let actual =
            studio_runtime_services::claude_login::chats::refreshed_agent(&row["input"], "account")
                .unwrap_or_else(|| row["input"].clone());
        assert_eq!(actual, row["expected"]);
    }
    for field in ["requestId", "status", "accountKey"] {
        let mut record = corpus["rows"][0]["input"].clone();
        record["receipt"][field] = serde_json::json!([]);
        assert!(LoginSnapshot::from_record(record).is_err());
    }
    for row in corpus["urls"].as_array().expect("valid fixture") {
        assert_eq!(
            verification_url(row["value"].as_str().expect("valid fixture")),
            row["expected"].as_bool().expect("valid fixture")
        );
    }
    for row in corpus["codes"].as_array().expect("valid fixture") {
        assert_eq!(
            validate_code(row["value"].as_str().expect("valid fixture")).is_ok(),
            row["expected"].as_bool().expect("valid fixture")
        );
    }
}
