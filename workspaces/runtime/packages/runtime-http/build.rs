#![allow(
    clippy::disallowed_methods,
    reason = "build script runs in a dedicated compiler process, outside Tokio"
)]
use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

use openapi_to_rust::analysis::SchemaType;
use openapi_to_rust::{CodeGenerator, GeneratorConfig, SchemaAnalyzer};
fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    // Clippy does not merge parent configs: runtime/clippy.toml owns the shared policy.
    let shared_clippy = manifest_dir.join("../../clippy.toml");
    let http_clippy = manifest_dir.join("clippy.toml");
    println!("cargo:rerun-if-changed={}", shared_clippy.display());
    println!("cargo:rerun-if-changed={}", http_clippy.display());
    let shared = fs::read_to_string(shared_clippy)?;
    let http = fs::read_to_string(http_clippy)?;
    let inherited = http
        .lines()
        .filter(|line| {
            !line.contains("HTTP-only blocking filesystem guards")
                && !line.contains("HTTP handlers must use tokio::fs")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_eq!(
        shared, inherited,
        "HTTP Clippy config must preserve the policy owned by runtime/clippy.toml"
    );
    let spec_path = manifest_dir.join("openapi.json");
    let output_dir = PathBuf::from(env::var("OUT_DIR")?).join("generated");
    println!("cargo:rerun-if-changed={}", spec_path.display());
    println!("cargo:rerun-if-changed=build.rs");

    let document: serde_json::Value = serde_json::from_slice(&fs::read(&spec_path)?)?;
    let mut analyzer = SchemaAnalyzer::new(document)?;
    let mut analysis = analyzer.analyze()?;
    // Keep the wire schema shared, and deserialize ingress timing directly
    // into its runtime owner through the explicit compatibility codec.
    let SchemaType::Object { properties, .. } = &mut analysis
        .schemas
        .get_mut("ManagedMessageBody")
        .ok_or("missing managed message schema")?
        .schema_type
    else {
        return Err("managed message schema must be an object".into());
    };
    properties
        .get_mut("delivery")
        .ok_or("missing managed message delivery")?
        .schema_type = SchemaType::Primitive {
        rust_type: "studio_runtime_schema::DeliveryMoment".to_owned(),
        serde_with: Some("studio_runtime_schema::delivery::compatibility".to_owned()),
    };
    let config = GeneratorConfig {
        spec_path: spec_path.clone(),
        output_dir: output_dir.clone(),
        module_name: "generated".to_owned(),
        enable_async_client: false,
        enable_sse_client: false,
        tracing_enabled: false,
        ..GeneratorConfig::default()
    };
    let generator = CodeGenerator::new(config)
        .with_source_provenance("workspaces/runtime/packages/runtime-http/openapi.json");
    let generated = generator.generate_all(&mut analysis)?;
    let mut module_lines = Vec::new();
    let mut modules = Vec::new();
    for file in generated.files {
        let path = output_dir.join(file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let module = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or("generated module has an invalid filename")?;
        modules.push(module.to_owned());
        module_lines.push(format!("#[path = {:?}] pub mod {module};", path));
        fs::write(path, file.content)?;
    }
    if modules.iter().any(|module| module == "types") {
        module_lines.push("pub use types::*;".to_owned());
    }
    fs::write(output_dir.join("mod.rs"), module_lines.join("\n") + "\n")?;
    Ok(())
}
