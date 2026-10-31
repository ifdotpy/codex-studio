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
        // DIAGNOSTIC (remove before merge): show how a stale file differs from its rendering.
        #[allow(clippy::disallowed_methods)]
        for file in rendered.iter().filter(|file| stale.contains(&file.path)) {
            let expected = std::env::temp_dir().join("contract-expected");
            if std::fs::write(&expected, &file.text).is_ok()
                && let Ok(out) = std::process::Command::new("diff")
                    .arg("-u")
                    .arg(root.join(&file.path))
                    .arg(&expected)
                    .output()
            {
                for line in String::from_utf8_lossy(&out.stdout).lines().take(900) {
                    eprintln!("DIFF {}: {line}", file.path);
                }
            }
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
