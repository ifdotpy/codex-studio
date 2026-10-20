use studio_runtime_services::claude_login::{validate_code, validate_manual_authorization_code};

#[test]
fn manual_authorization_code_shape_is_validated() {
    for (input, expected) in [
        ("fixture-code#fixture-state", true),
        (" fixture-code#fixture-state ", true),
        ("fixture-code#fixture-state#extra", false),
        ("fixture-code", false),
        ("#fixture-state", false),
        ("fixture-code#", false),
        (" #fixture-state", false),
        ("fixture-code#   ", false),
        ("   #fixture-state", false),
        ("fixture-code#   #extra", false),
    ] {
        assert_eq!(
            validate_manual_authorization_code(input).is_ok(),
            expected,
            "manual authorization code shape: {input:?}"
        );
    }
    // This remains a provider-specific guard; the shared printable validator
    // retains its prior contract for other callers.
    assert!(validate_code("fixture-code").is_ok());
}
