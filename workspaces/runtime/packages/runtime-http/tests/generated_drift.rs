use std::path::PathBuf;

use openapi_to_rust::{CodeGenerator, GeneratorConfig, SchemaAnalyzer};

#[test]
fn checked_in_dtos_match_the_openapi_generator() {
    assert_eq!(openapi_to_rust::VERSION, "0.22.0");
    let document: serde_json::Value = serde_json::from_str(include_str!("../openapi.json"))
        .expect("committed canonical OpenAPI must be valid JSON");
    let mut analyzer = SchemaAnalyzer::new(document).expect("OpenAPI model must parse");
    let mut analysis = analyzer.analyze().expect("OpenAPI schemas must analyze");
    let config = GeneratorConfig {
        spec_path: PathBuf::from("workspaces/runtime/packages/runtime-http/openapi.json"),
        output_dir: PathBuf::from("workspaces/runtime/packages/runtime-http/src/generated"),
        module_name: "generated".to_owned(),
        enable_async_client: false,
        enable_sse_client: false,
        tracing_enabled: false,
        ..GeneratorConfig::default()
    };
    let generator = CodeGenerator::new(config)
        .with_source_provenance("workspaces/runtime/packages/runtime-http/openapi.json");
    let output = generator
        .generate_all(&mut analysis)
        .expect("DTO generation must succeed");
    let expected = output
        .files
        .iter()
        .find(|file| file.path.file_name().is_some_and(|name| name == "types.rs"))
        .expect("types-only generation must emit types.rs");
    assert_eq!(expected.content, include_str!("../src/generated/types.rs"));
}
