pub(crate) fn prepared_python(source: &std::path::Path) -> std::path::PathBuf {
    std::env::var_os("CODEX_AGENTS_PYTHON")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let output = std::process::Command::new("python3")
                .arg(source.join("codex_python.py"))
                .output()
                .expect("resolve prepared Python interpreter");
            assert!(
                output.status.success(),
                "prepared Python interpreter resolves: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::path::PathBuf::from(
                String::from_utf8(output.stdout)
                    .expect("Python interpreter path is UTF-8")
                    .trim(),
            )
        })
}
