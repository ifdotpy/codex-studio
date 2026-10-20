//! Catalog read projections of provider responses and the managed tool inventory.

use serde_json::json;
use studio_runtime_services::catalog_reads::{managed_tool_catalog, skills_for_cwd};
use studio_runtime_services::thread_prepare::{CatalogSnapshot, CatalogSource, ToolDefinition};

#[test]
fn malformed_skill_responses_become_errors_without_skills() {
    for malformed in [
        json!({"data":[{"cwd":"/repo","errors":null,"skills":[{"name":"hidden","path":"/hidden"}]}]}),
        json!({"data":[{"cwd":"/repo","skills":null}]}),
    ] {
        let result = skills_for_cwd("/repo", &malformed)
            .unwrap_or_else(|message| json!({"skills":[],"errors":[message]}));
        assert_eq!(result["skills"], json!([]));
        assert!(!result["errors"].as_array().expect("errors list").is_empty());
    }
}

#[test]
fn managed_tool_catalog_serializes_the_full_selected_inventory() {
    let catalog = CatalogSnapshot {
        source: CatalogSource::Current,
        definitions: vec![ToolDefinition {
            name: "fixture-tool".into(),
            description: "fixture description".into(),
            input_schema: serde_json::from_value(json!({"type":"object"})).expect("fixture schema"),
            extra: serde_json::from_value(json!({"strict":true})).expect("fixture extension"),
        }],
        saved_thread_id: None,
        saved_digest: None,
    };
    assert_eq!(
        managed_tool_catalog(&catalog).expect("full selected tool inventory"),
        vec![json!({
            "name":"fixture-tool",
            "description":"fixture description",
            "inputSchema":{"type":"object"},
            "strict":true
        })]
    );
}
