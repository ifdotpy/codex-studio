use std::{process::Stdio, time::Duration};
use studio_runtime_store::RuntimeStore;

#[tokio::test]
async fn migration_startup_failure_prevents_listener_and_readiness() {
    let state = tempfile::tempdir().expect("state");
    drop(RuntimeStore::open(Some(state.path())).await.expect("fresh"));
    std::fs::create_dir(state.path().join("accounts")).expect("accounts");
    std::fs::write(
        state.path().join("accounts/registry.json"),
        br#"{"version":2,"accounts":{"broken":{}},"logins":{}}"#,
    )
    .expect("invalid current registry");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_studio-runtime-server"));
    command
        .args(["--port", "0"])
        .env("CODEX_AGENTS_STATE_DIR", state.path())
        .env("CODEX_STUDIO_STARTUP_TRACE", "1")
        .env_remove("CODEX_BACKEND_BUILD")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("bounded startup")
        .expect("process result");
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(stderr.contains("store-open-started"), "{stderr}");
    assert!(stderr.contains("2026-10-26-account-registry"), "{stderr}");
    assert!(!stderr.contains("listener-bound"), "{stderr}");
    assert!(
        !stderr.contains("account-registry-open-started"),
        "strict consumers must not start: {stderr}"
    );
    assert!(!stdout.contains("Codex Canvas:"), "{stdout}");
}
