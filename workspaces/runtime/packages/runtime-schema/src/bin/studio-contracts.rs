//! Render or check the files generated from the Rust-owned contracts.
//! Usage: `studio-contracts --check` or `studio-contracts --write`.

use std::process::ExitCode;

use studio_runtime_schema::contracts;

fn main() -> ExitCode {
    let write = match std::env::args().nth(1).as_deref() {
        Some("--check") => false,
        Some("--write") => true,
        _ => {
            eprintln!("usage: studio-contracts --check | --write");
            return ExitCode::from(2);
        }
    };
    let root = contracts::repository_root();
    let rendered = match contracts::render(&root, true) {
        Ok(rendered) => rendered,
        Err(error) => {
            eprintln!("contract rendering failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let stale = contracts::stale(&root, &rendered);
    if !write {
        for path in &stale {
            eprintln!("{path} is stale; run `pnpm run contracts:generate`");
        }
        return if stale.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    for file in rendered.iter().filter(|file| stale.contains(&file.path)) {
        if let Err(error) = std::fs::write(root.join(&file.path), &file.text) {
            eprintln!("{}: {error}", file.path);
            return ExitCode::FAILURE;
        }
        println!("wrote {}", file.path);
    }
    ExitCode::SUCCESS
}
