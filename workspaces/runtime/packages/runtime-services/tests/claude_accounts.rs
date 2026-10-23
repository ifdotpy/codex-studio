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
        organization_name: None,
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
            "orgId":org,"orgName":"Fixture organization","accountUuid":account}))
        .expect("Claude status serializes"),
        true,
    )
}

#[test]
fn same_email_two_organizations_have_different_credential_identities() {
    let first = organization_metadata("org-a", Some("account-a"));
    assert_eq!(
        first.organization_name.as_deref(),
        Some("Fixture organization")
    );
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
    let dir = tempfile::tempdir().expect("temporary account directory creates");
    let path = dir.path().join("registry.json");
    std::fs::write(&path, json!({"version":1,"logins":{},"defaultAccountKey":"default",
        "accounts":{"default":{"id":"default","home":"/fixture/codex","label":"Codex","source":"Local profile","status":"signedOut"}}}).to_string()).expect("registry fixture writes");
    let driver = AccountDriver::open_with_reader(&path, NoProbe).expect("account driver opens");
    let options = json!({"binaryPath":"/fixture/claude","configDir":"/fixture/config"})
        .as_object()
        .expect("Claude options are an object")
        .clone();
    (dir, driver, options)
}

#[test]
fn changed_organization_is_refused_and_original_pin_survives() {
    let (dir, driver, options) = fixture();
    let first = organization_metadata("org-a", Some("account-a"));
    let key = driver
        .register_claude_verified(options.clone(), "Fixture", dir.path(), &first)
        .expect("verified account registers");
    let profile = driver.claude_profile(&key).expect("Claude profile loads");
    let changed = organization_metadata("org-b", Some("account-a"));
    let result = driver
        .refresh_claude_verified(&key, &profile, &changed, true)
        .expect("changed identity refresh completes");
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
        .expect("original identity refresh completes");
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
        .expect("legacy account registers");
    let legacy = driver.claude_profile(&key).expect("legacy profile loads");
    let observed = organization_metadata("org-a", None);
    let upgraded = driver
        .refresh_claude_verified(&key, &legacy, &observed, false)
        .expect("identity upgrade completes");
    assert_eq!(upgraded["status"], "ready");
    assert_ne!(
        upgraded["_credentialIdentity"],
        legacy["_credentialIdentity"]
    );
    drop(driver);
    let driver = AccountDriver::open_with_reader(dir.path().join("registry.json"), NoProbe)
        .expect("account driver reopens");
    assert_eq!(
        driver
            .claude_profile(&key)
            .expect("upgraded profile reloads"),
        upgraded
    );
    // A caller that retained the legacy record cannot replace the durable pin.
    let stale = driver
        .refresh_claude_verified(&key, &legacy, &organization_metadata("org-b", None), false)
        .expect("stale profile refresh completes");
    assert_eq!(stale["status"], "changed");
    assert_eq!(
        stale["_credentialIdentity"],
        upgraded["_credentialIdentity"]
    );
    let observed = organization_metadata("org-a", Some("account-a"));
    let complete = driver
        .refresh_claude_verified(&key, &upgraded, &observed, false)
        .expect("complete identity refresh completes");
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
            .expect("candidate identity refresh completes");
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
        .expect("legacy account registers");
    let observed = organization_metadata("org-a", Some("account-a"));
    assert_eq!(
        driver
            .register_claude_verified(options.clone(), "Fixture", dir.path(), &observed)
            .expect("verified account registration reuses identity"),
        key
    );
    assert_eq!(
        driver.claude_profile(&key).expect("verified profile loads")["_credentialIdentity"],
        observed
            .credential_identity
            .expect("verified credential identity exists")
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

#[test]
fn public_organization_metadata_keeps_pins_and_excludes_private_identity() {
    let (dir, driver, options) = fixture();
    let mut original = organization_metadata(
        "organization-private-id-12345678",
        Some("private-account-uuid"),
    );
    original.organization_name = Some("Expected organization".into());
    let key = driver
        .register_claude_verified(options, "Fixture", dir.path(), &original)
        .expect("organization profile registers");
    let profile = driver
        .claude_profile(&key)
        .expect("registered profile exists");
    let mut changed = organization_metadata(
        "organization-private-id-87654321",
        Some("other-account-uuid"),
    );
    changed.organization_name = Some("Observed organization".into());
    let refused = driver
        .refresh_claude_verified(&key, &profile, &changed, true)
        .expect("organization mismatch persists");
    assert_eq!(refused["status"], "changed");
    let snapshot = driver.snapshot().expect("account snapshot reads");
    let account = snapshot
        .accounts
        .iter()
        .find(|a| a["id"] == key)
        .expect("registered account is listed");
    assert_eq!(
        account["organization"],
        json!({"name":"Expected organization","idSuffix":"12345678"})
    );
    assert_eq!(
        account["organizationMismatch"]["observed"],
        json!({"name":"Observed organization","idSuffix":"87654321"})
    );
    let public = serde_json::to_string(account).expect("public account serializes");
    for secret in [
        "_credentialIdentity",
        "_organizationName",
        "private-account-uuid",
        "other-account-uuid",
        "organization-private-id-",
    ] {
        assert!(!public.contains(secret), "public account leaked {secret}");
    }
    driver
        .refresh_claude_verified(&key, &refused, &original, true)
        .expect("original organization restores");
    let restored = driver.snapshot().expect("restored snapshot reads");
    let account = restored
        .accounts
        .iter()
        .find(|a| a["id"] == key)
        .expect("restored account is listed");
    assert!(account.get("organizationMismatch").is_none());
}

#[test]
fn public_ui_contracts_match_persisted_validators() {
    // OpenAPI owns these public shapes. Check each persisted validator copy.
    let public: serde_json::Value =
        serde_json::from_str(include_str!("../../runtime-http/openapi.json"))
            .expect("OpenAPI schema parses");
    let accounts: serde_json::Value = serde_json::from_str(
        &include_str!("../../runtime-schema/src/schemas/account-registry.schema.json")
            .replace("#/$defs/", "#/components/schemas/"),
    )
    .expect("account validator parses");
    let limits: serde_json::Value = serde_json::from_str(
        &include_str!("../../runtime-schema/src/schemas/workspace-usage-limits.schema.json")
            .replace("#/$defs/", "#/components/schemas/"),
    )
    .expect("limits validator parses");
    for name in ["ClaudeOrganization", "ClaudeOrganizationMismatch"] {
        assert_eq!(
            public["components"]["schemas"][name], accounts["$defs"][name],
            "{name} must match its OpenAPI owner"
        );
    }
    assert_eq!(
        public["components"]["schemas"]["UsageEligibility"], limits["$defs"]["UsageEligibility"],
        "UsageEligibility must match its OpenAPI owner"
    );
}
