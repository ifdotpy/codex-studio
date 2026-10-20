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
