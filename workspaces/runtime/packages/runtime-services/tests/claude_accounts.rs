use serde_json::json;
use studio_runtime_services::account_driver::{
    AccountDriver, AuthMetadata, AuthProfile, AuthStatus, ProviderAuthReader,
};
struct NoProbe;
impl ProviderAuthReader for NoProbe {
    fn read(&self, _: &AuthProfile) -> AuthMetadata {
        panic!("verified APIs must not probe credentials");
    }
}
fn metadata(email: &str) -> AuthMetadata {
    AuthMetadata {
        status: AuthStatus::Ready,
        account_id: Some(format!("claude:{email}")),
        email: Some(email.into()),
        plan: Some("max".into()),
        credential_identity: Some(format!("claude:{email}")),
        error: None,
        error_kind: None,
    }
}
#[test]
fn register_refresh_and_conflict_without_credential_access() {
    let dir = tempfile::tempdir().expect("valid fixture");
    let path = dir.path().join("registry.json");
    std::fs::write(&path,serde_json::to_vec(&json!({"version":1,"logins":{},"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":"/fixture/codex","label":"Codex","source":"Local profile","status":"signedOut"}}})).expect("valid fixture")).expect("valid fixture");
    let driver = AccountDriver::open_with_reader(&path, NoProbe).expect("valid fixture");
    let options = json!({"binaryPath":"/fixture/scripted-claude","configDir":"/fixture/config"})
        .as_object()
        .expect("valid fixture")
        .clone();
    let auth = metadata("fixture@example.test");
    let key = driver
        .register_claude_verified(options.clone(), "Fixture", dir.path(), &auth)
        .expect("valid fixture");
    assert_eq!(
        driver
            .register_claude_verified(options.clone(), "Fixture", dir.path(), &auth)
            .expect("valid fixture"),
        key
    );
    let profile = driver.claude_profile(&key).expect("valid fixture");
    assert_eq!(
        driver
            .find_claude_config("/fixture/config")
            .expect("valid fixture"),
        Some(profile.clone())
    );
    let refreshed = driver
        .refresh_claude_verified(&key, &profile, &auth, true)
        .expect("valid fixture");
    assert_eq!(refreshed["status"], "ready");
    let mut wrong = profile.clone();
    wrong["accountId"] = json!("claude:other@example.test");
    let before = std::fs::read(&path).expect("valid fixture");
    assert!(
        driver
            .refresh_claude_verified(&key, &wrong, &auth, true)
            .is_err()
    );
    assert_eq!(std::fs::read(&path).expect("valid fixture"), before);
    assert!(
        driver
            .register_claude_verified(
                options,
                "Fixture",
                dir.path(),
                &metadata("other@example.test")
            )
            .is_err()
    );
    assert_eq!(std::fs::read(&path).expect("valid fixture"), before);
}
