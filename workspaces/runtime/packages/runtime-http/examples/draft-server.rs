//! Loopback server for the web package's real RxDB replication witness.

use std::{io::Write, sync::Arc};
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::{RuntimeStore, SyncDocumentsRepository};

#[tokio::main]
async fn main() {
    // Vitest owns the directory so even forced fixture cleanup removes state.
    let directory = std::path::PathBuf::from(std::env::args_os().nth(1).expect("state directory"));
    let store = Arc::new(RuntimeStore::open(Some(&directory)).await.expect("store"));
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../client/contracts/draft-replication.json"
    ))
    .expect("shared draft fixture");
    store
        .transaction(move |tx| {
            for payload in fixture["legacy"].as_array().expect("legacy rows") {
                SyncDocumentsRepository::put_draft(
                    tx,
                    payload["id"].as_str().expect("id"),
                    &payload.to_string(),
                    false,
                )?;
            }
            Ok(())
        })
        .await
        .expect("historical drafts");
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
        directory.display().to_string(),
        address.port(),
        None,
        Some(store),
        None,
    );
    let app = router(state).expect("real router");
    println!(
        "{}",
        serde_json::json!({"port": address.port(), "workspaceId": workspace_id})
    );
    std::io::stdout().flush().expect("fixture readiness");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/examples/draft-server",
            || {
                use std::io::Read;
                let _closed = std::io::stdin().read(&mut [0]);
            },
        )
        .await
        .expect("fixture shutdown reader");
    })
    .await
    .expect("fixture server");
}
