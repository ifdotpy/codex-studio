use serde_json::json;
#[test]
fn bridge_source_fingerprint_and_generation_compatibility_are_conservative() {
    use studio_runtime_services::claude_bridge::{bridge_build, compatibility_hold};
    let root = tempfile::tempdir().expect("fixture");
    for (name, body) in [
        ("z.mjs", "last"),
        ("a.mjs", "first: café"),
        ("x.test.mjs", "ignored test"),
        ("ignore.js", "ignored extension"),
    ] {
        std::fs::write(root.path().join(name), body).expect("fixture source");
    }
    let expected = bridge_build(root.path()).expect("fingerprint");
    assert_eq!(bridge_build(root.path()).expect("fingerprint"), expected);
    std::fs::write(root.path().join("x.test.mjs"), "changed ignored test").expect("test edit");
    assert_eq!(bridge_build(root.path()).expect("fingerprint"), expected);
    let init = json!({"capabilities":{"claudeVersion":23,"claudeBuild":expected}});
    assert!(compatibility_hold(&init, &expected, &json!({}), &json!({})).is_none());
    assert!(
        compatibility_hold(
            &init,
            &expected,
            &json!({}),
            &json!({"permissionMode":"plan"})
        )
        .is_some()
    );
    assert!(
        compatibility_hold(
            &json!({"capabilities":{"claudeVersion":22,"claudeBuild":expected}}),
            &expected,
            &json!({}),
            &json!({})
        )
        .is_some()
    );
    std::fs::write(root.path().join("a.mjs"), "changed production source").expect("source edit");
    let changed = bridge_build(root.path()).expect("changed fingerprint");
    assert_ne!(changed, expected);
    assert!(compatibility_hold(&init, &changed, &json!({}), &json!({})).is_some());
}

#[test]
fn bridge_launch_pins_organization_and_refuses_another_organization() {
    use studio_runtime_services::account_driver::claude::auth_metadata_from_output;
    use studio_runtime_services::claude_bridge::{BridgeSnapshot, ResolvedBridge};
    let metadata = |org| {
        auth_metadata_from_output(
            &serde_json::to_vec(&json!({
                "loggedIn":true,"authMethod":"claude.ai","email":"fixture@example.test",
                "orgId":org,"accountUuid":"account-a","subscriptionType":"max"
            }))
            .expect("Claude status serializes"),
            true,
        )
    };
    let first = metadata("org-a");
    let profile = json!({"provider":"claude","status":"ready","email":first.email,
        "accountId":first.account_id,"_credentialIdentity":first.credential_identity,
        "claudeOptions":{}});
    let snapshot = BridgeSnapshot::from_record(profile).expect("bridge snapshot parses");
    let paths = || ResolvedBridge {
        node: "/fixture/node".into(),
        executable: "/fixture/claude".into(),
        bridge: "/fixture/bridge.mjs".into(),
        root: "/fixture/root".into(),
        sdk_present: true,
    };
    let launch = snapshot
        .launch(paths(), &first, Default::default())
        .expect("verified bridge launch succeeds");
    assert_eq!(
        launch.environment["STUDIO_CLAUDE_ACCOUNT"],
        "fixture@example.test"
    );
    assert_eq!(
        Some(&launch.environment["STUDIO_CLAUDE_CREDENTIAL_IDENTITY"]),
        first.credential_identity.as_ref()
    );
    assert!(
        snapshot
            .launch(paths(), &metadata("org-b"), Default::default())
            .is_err()
    );
}
