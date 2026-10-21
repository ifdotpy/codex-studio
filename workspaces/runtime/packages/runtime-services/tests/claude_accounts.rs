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

fn organization_metadata(org: &str, account: Option<&str>) -> AuthMetadata {
    studio_runtime_services::account_driver::claude::auth_metadata_from_output(
        &serde_json::to_vec(&json!({"loggedIn":true,"authMethod":"claude.ai",
            "email":"fixture@example.test","subscriptionType":"max",
            "orgId":org,"accountUuid":account}))
        .unwrap(),
        true,
    )
}

#[test]
fn same_email_two_organizations_have_different_credential_identities() {
    let first = organization_metadata("org-a", Some("account-a"));
    let second = organization_metadata("org-b", Some("account-a"));
    assert_ne!(first.credential_identity, second.credential_identity);
    assert_eq!(first.account_id, second.account_id);
    assert_ne!(
        first.credential_identity,
        organization_metadata("org-a", Some("account-b")).credential_identity
    );
}

fn fixture() -> (
    tempfile::TempDir,
    AccountDriver<NoProbe>,
    serde_json::Map<String, serde_json::Value>,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.json");
    std::fs::write(&path, json!({"version":1,"logins":{},"defaultAccountKey":"default",
        "accounts":{"default":{"id":"default","home":"/fixture/codex","label":"Codex","source":"Local profile","status":"signedOut"}}}).to_string()).unwrap();
    let driver = AccountDriver::open_with_reader(&path, NoProbe).unwrap();
    let options = json!({"binaryPath":"/fixture/claude","configDir":"/fixture/config"})
        .as_object()
        .unwrap()
        .clone();
    (dir, driver, options)
}

#[test]
fn changed_organization_is_refused_and_original_pin_survives() {
    let (dir, driver, options) = fixture();
    let first = organization_metadata("org-a", Some("account-a"));
    let key = driver
        .register_claude_verified(options.clone(), "Fixture", dir.path(), &first)
        .unwrap();
    let profile = driver.claude_profile(&key).unwrap();
    let changed = organization_metadata("org-b", Some("account-a"));
    let result = driver
        .refresh_claude_verified(&key, &profile, &changed, true)
        .unwrap();
    assert_eq!(result["status"], "changed");
    assert_eq!(
        result["_credentialIdentity"],
        profile["_credentialIdentity"]
    );
    assert!(
        driver
            .register_claude_verified(options, "Fixture", dir.path(), &changed)
            .is_err()
    );
    let restored = driver
        .refresh_claude_verified(&key, &result, &first, true)
        .unwrap();
    assert_eq!(restored["status"], "ready");
}

#[test]
fn legacy_email_identity_upgrades_once_and_persists_across_restart() {
    let (dir, driver, options) = fixture();
    let key = driver
        .register_claude_verified(
            options,
            "Fixture",
            dir.path(),
            &metadata("fixture@example.test"),
        )
        .unwrap();
    let legacy = driver.claude_profile(&key).unwrap();
    let observed = organization_metadata("org-a", None);
    let upgraded = driver
        .refresh_claude_verified(&key, &legacy, &observed, false)
        .unwrap();
    assert_eq!(upgraded["status"], "ready");
    assert_ne!(
        upgraded["_credentialIdentity"],
        legacy["_credentialIdentity"]
    );
    drop(driver);
    let driver =
        AccountDriver::open_with_reader(&dir.path().join("registry.json"), NoProbe).unwrap();
    assert_eq!(driver.claude_profile(&key).unwrap(), upgraded);
    // A caller that retained the legacy record cannot replace the durable pin.
    let stale = driver
        .refresh_claude_verified(&key, &legacy, &organization_metadata("org-b", None), false)
        .unwrap();
    assert_eq!(stale["status"], "changed");
    assert_eq!(
        stale["_credentialIdentity"],
        upgraded["_credentialIdentity"]
    );
    let observed = organization_metadata("org-a", Some("account-a"));
    let complete = driver
        .refresh_claude_verified(&key, &upgraded, &observed, false)
        .unwrap();
    assert_eq!(complete["status"], "ready");
    assert_ne!(
        complete["_credentialIdentity"],
        upgraded["_credentialIdentity"]
    );
    for candidate in [
        organization_metadata("org-b", Some("account-a")),
        organization_metadata("org-a", Some("account-b")),
        metadata("fixture@example.test"),
    ] {
        let result = driver
            .refresh_claude_verified(&key, &complete, &candidate, false)
            .unwrap();
        assert_eq!(result["status"], "changed");
        assert_eq!(
            result["_credentialIdentity"],
            complete["_credentialIdentity"]
        );
    }
}

#[test]
fn registration_retry_upgrades_legacy_identity_without_rebinding() {
    let (dir, driver, options) = fixture();
    let legacy = metadata("fixture@example.test");
    let key = driver
        .register_claude_verified(options.clone(), "Fixture", dir.path(), &legacy)
        .unwrap();
    let observed = organization_metadata("org-a", Some("account-a"));
    assert_eq!(
        driver
            .register_claude_verified(options.clone(), "Fixture", dir.path(), &observed)
            .unwrap(),
        key
    );
    assert_eq!(
        driver.claude_profile(&key).unwrap()["_credentialIdentity"],
        observed.credential_identity.unwrap()
    );
    assert!(
        driver
            .register_claude_verified(
                options,
                "Fixture",
                dir.path(),
                &organization_metadata("org-b", Some("account-a"))
            )
            .is_err()
    );
}
