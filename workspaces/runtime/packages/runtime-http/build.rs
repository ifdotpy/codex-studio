use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

use openapi_to_rust::{CodeGenerator, GeneratorConfig, SchemaAnalyzer};
fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let spec_path = manifest_dir.join("openapi.json");
    let output_dir = PathBuf::from(env::var("OUT_DIR")?).join("generated");
    println!("cargo:rerun-if-changed={}", spec_path.display());
    println!("cargo:rerun-if-changed=build.rs");

    let document: serde_json::Value = serde_json::from_slice(&fs::read(&spec_path)?)?;
    let mut analyzer = SchemaAnalyzer::new(document)?;
    let mut analysis = analyzer.analyze()?;
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
