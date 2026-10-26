use serde_json::json;
use studio_runtime_services::title_generation::title_model;

#[test]
fn title_model_prefers_luna_when_no_mini_is_available() {
    let agent = json!({"provider":"codex","model":"gpt-6"});
    let catalog = json!({"data":[{"model":"gpt-6"},{"model":"luna-fast"}]});
    assert_eq!(
        title_model(&catalog, &agent).expect("fallback model"),
        ("luna-fast".into(), None)
    );
}
