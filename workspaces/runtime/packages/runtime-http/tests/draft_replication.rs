//! Exercise the renderer's actual RxDB replication against the HTTP router.

use std::{path::Path, sync::Arc, time::Duration};
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::{RuntimeStore, SyncDocumentsRepository};

#[tokio::test]
async fn real_rxdb_single_writer_draft_ack_echo_ordering() {
    let directory = tempfile::tempdir().expect("isolated draft store");
    let store = Arc::new(
        RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store"),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let address = listener.local_addr().expect("address");
    let workspace_id = store
        .read_async("draft witness identity", |tx| {
            SyncDocumentsRepository::workspace_id(tx)
        })
        .await
        .expect("workspace identity");
    let state = HttpState::new(
        "draft-witness".into(),
        workspace_id.clone(),
        directory.path().display().to_string(),
        address.port(),
        None,
        Some(store),
        None,
    );
    let app = router(state).expect("real router");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    });
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new("pnpm")
            .current_dir(repository)
            .args([
                "--filter",
                "codex-agents-web",
                "exec",
                "vitest",
                "run",
                "--config",
                "vitest.unit.config.ts",
                "src/sync/client.draftServer.test.ts",
            ])
            .env("STUDIO_DRAFT_WITNESS_URL", format!("http://{address}"))
            .env("STUDIO_DRAFT_WITNESS_WORKSPACE", workspace_id)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    server.abort();
    let _cancelled = server.await;
    let output = output
        .expect("bounded RxDB witness")
        .expect("vitest starts");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stdout}");
    assert!(
        output.status.success(),
        "RxDB/server witness failed:\n{stdout}\n{stderr}"
    );
}
