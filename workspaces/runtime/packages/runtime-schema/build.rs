// The TypeScript generator owns freshness; Cargo must never compile stale wire
// types after a committed vendor schema changes. No CLI, network or state is used.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let root = manifest.join("../../../..");
    let generator = "workspaces/tooling/apps/repository-checks/generate-provider-contracts.ts";
    for file in [
        generator,
        "workspaces/tooling/apps/repository-checks/provider-contract-sources.ts",
        "workspaces/runtime/packages/runtime-schema/src/codex_generated.rs",
        "workspaces/runtime/packages/runtime-schema/src/schemas/codex",
    ] {
        println!("cargo:rerun-if-changed={}", root.join(file).display());
    }
    let mut child = std::process::Command::new("node")
        .arg(root.join(generator))
        .args(["--check", "--rust-only"])
        .spawn()?;
    // Cargo runs this check on its dedicated build-script process, outside the
    // runtime. Bound the checker without introducing a runtime dependency.
    const CHECK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
    const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= CHECK_DEADLINE {
            child.kill()?;
            child.wait()?;
            return Err("Codex wire contract freshness check timed out".into());
        }
        std::thread::park_timeout(CHECK_INTERVAL);
    };
    if !status.success() {
        return Err(
            "Codex wire contracts are stale; run pnpm run contracts:vendor:generate".into(),
        );
    }
    Ok(())
}
