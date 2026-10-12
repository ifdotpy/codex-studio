use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

use openapi_to_rust::{CodeGenerator, GeneratorConfig, SchemaAnalyzer};

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args().skip(1).collect::<Vec<_>>();
    let check = arguments.first().is_some_and(|arg| arg == "--check");
    if check {
        arguments.remove(0);
    }
    let mut arguments = arguments.into_iter();
    let spec_path = PathBuf::from(
        arguments
            .next()
            .unwrap_or_else(|| "workspaces/runtime/packages/runtime-http/openapi.json".to_owned()),
    );
    let output_dir =
        PathBuf::from(arguments.next().unwrap_or_else(|| {
            "workspaces/runtime/packages/runtime-http/src/generated".to_owned()
        }));
    if arguments.next().is_some() {
        return Err("usage: studio-runtime-http-codegen [--check] [spec] [output-dir]".into());
    }

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
    let mut mismatches = Vec::new();
    for file in generated.files {
        let path = output_dir.join(&file.path);
        if check {
            let current = fs::read_to_string(&path).unwrap_or_default();
            if current != file.content {
                mismatches.push(path);
            }
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, file.content)?;
        }
    }
    if !mismatches.is_empty() {
        return Err(format!(
            "Rust DTOs are stale; rerun `pnpm run api:generate`: {}",
            mismatches
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    }
    Ok(())
}
