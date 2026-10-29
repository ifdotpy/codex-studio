use std::{process::Stdio, time::Duration};
use studio_runtime_store::RuntimeStore;

#[tokio::test]
async fn startup_serves_health_and_refuses_commands_while_database_is_busy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let state = tempfile::tempdir().expect("state");
    drop(RuntimeStore::open(Some(state.path())).await.expect("fresh"));
    let db = rusqlite::Connection::open(state.path().join("canvas.sqlite3")).expect("database");
    db.execute_batch("BEGIN IMMEDIATE")
        .expect("hold migration writer");
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("port");
    let port = reservation.local_addr().expect("address").port();
    drop(reservation);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_studio-runtime-server"))
        .args(["--port", &port.to_string()])
        .env("CODEX_AGENTS_STATE_DIR", state.path())
        .env_remove("CODEX_BACKEND_BUILD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("server");
    async fn request(port: u16, request: &str) -> String {
        // Like the sibling startup tests, allow 30 s: the host may scan a freshly linked
        // binary for several seconds at its first launch, before the server runs.
        let mut stream = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                    Ok(stream) => break stream,
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("listener");
        stream.write_all(request.as_bytes()).await.expect("request");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("response");
        response
    }
    let status = request(
        port,
        "GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        status.contains("200 OK") && status.contains("migrating"),
        "{status}"
    );
    let command = request(port, "POST /api/commands HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
    assert!(
        command.contains("503 Service Unavailable") && command.contains("migrating"),
        "{command}"
    );
    child.kill().await.expect("interrupt server");
    db.execute_batch("ROLLBACK").expect("release writer");
    drop(db);
    drop(
        RuntimeStore::open(Some(state.path()))
            .await
            .expect("resume"),
    );
}

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

#[tokio::test]
async fn migration_startup_unclassified_receipt_prevents_all_recovery() {
    let state = tempfile::tempdir().expect("state");
    drop(RuntimeStore::open(Some(state.path())).await.expect("fresh"));
    let db = rusqlite::Connection::open(state.path().join("canvas.sqlite3")).expect("database");
    db.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL); INSERT INTO runtime_operation_receipts VALUES('unsupported-operation','unsupported-signature','{}')").expect("unclassifiable source");
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
        .expect("process");
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("store-open-started"), "{stderr}");
    for expected in [
        "runtime_operation_receipts",
        "unsupported-operation",
        "unsupported-signature",
    ] {
        assert!(
            stderr.contains(expected),
            "{expected} missing from startup error: {stderr}"
        );
    }
    assert!(
        !stderr.contains("account-registry-open-started"),
        "provider recovery must not start: {stderr}"
    );
    assert!(!stderr.contains("listener-bound"), "{stderr}");
    assert_eq!(db.query_row("SELECT count(*) FROM runtime_operation_receipts WHERE id='unsupported-operation' AND signature='unsupported-signature'",[],|row|row.get::<_,i64>(0)).expect("source retained"),1);
}
