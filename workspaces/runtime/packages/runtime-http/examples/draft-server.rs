//! Loopback server for the web package's real RxDB replication witness.

use std::{io::Write, sync::Arc};
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::{
    AgentRecord, AgentRepository, RuntimeStore, StoreError, SyncDocumentsRepository,
};

/// Opt-in source writer for the entity SSE/client boundary witness. This route
/// exists only on this loopback fixture and requires its fixture token.
async fn write_entity(
    axum::extract::State(store): axum::extract::State<Arc<RuntimeStore>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<axum::Json<serde_json::Value>, axum::http::StatusCode> {
    if headers
        .get("x-canvas-token")
        .and_then(|value| value.to_str().ok())
        != Some("draft-witness")
    {
        return Err(axum::http::StatusCode::FORBIDDEN);
    }
    store.transaction_with(move |tx| {
        if let Some(agent) = body.get("agent") {
            AgentRepository::put(tx, &AgentRecord::from_value(agent.clone())?)?;
        }
        if let Some(task) = body.get("task") {
            tx.execute("INSERT INTO runtime_tasks(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record", rusqlite::params![task["id"].as_str().expect("fixture task id"),task.to_string()])?;
        }
        if body.get("appendWorkResult").and_then(serde_json::Value::as_bool) == Some(true) {
            let result = serde_json::json!({"id":"appended","agent":"lead","text":"New evidence only","checks":"fixture","revision":"fixture","files":[],"created":2});
            tx.execute("UPDATE runtime_work SET record=json_set(json_insert(record,'$.results[#]',json(?1)),'$.version',3,'$.updated',3) WHERE id='transfer-assignment'", [result.to_string()])?;
        }
        if let Some(status) = body.get("workStatus").and_then(serde_json::Value::as_str) {
            tx.execute("UPDATE runtime_work SET record=json_set(record,'$.status',?1,'$.updated',2,'$.version',2) WHERE id='transfer-assignment'", [status])?;
        }
        if let Some(project) = body.get("project") {
            studio_runtime_store::ProjectsRepository::put_project(tx, project)?;
        }
        Ok::<_, StoreError>(())
    }).await.map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let sequence = store
        .read_async("entity witness sequence", |tx| {
            SyncDocumentsRepository::entity_sequence(tx)
        })
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(axum::Json(serde_json::json!({"seq":sequence})))
}

/// Terminal source writes are opt-in and isolated to this test server.
async fn write_terminal(
    axum::extract::State(store): axum::extract::State<Arc<RuntimeStore>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<axum::Json<serde_json::Value>, axum::http::StatusCode> {
    if headers.get("x-canvas-token").and_then(|v| v.to_str().ok()) != Some("draft-witness") {
        return Err(axum::http::StatusCode::FORBIDDEN);
    }
    store
        .transaction_with(move |tx| {
            let id = body["id"]
                .as_str()
                .ok_or_else(|| StoreError::SchemaMismatch("fixture terminal id".into()))?;
            let record = studio_runtime_store::TerminalRecord::from_value(serde_json::json!({
                "id":id,"agent":"lead","title":"Shell","cwd":"/","created":1,
                "status":body["status"].as_str().unwrap_or("running")
            }))?;
            studio_runtime_store::TerminalsRepository::put(
                tx,
                id,
                &record,
                body["text"].as_str().unwrap_or_default(),
                body["offset"].as_i64().unwrap_or_default(),
            )
        })
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(axum::Json(serde_json::json!({"ok":true})))
}

#[tokio::main]
async fn main() {
    // Vitest owns the directory so even forced fixture cleanup removes state.
    let directory = std::path::PathBuf::from(std::env::args_os().nth(1).expect("state directory"));
    let store = Arc::new(RuntimeStore::open(Some(&directory)).await.expect("store"));
    let terminals = std::env::args().any(|arg| arg == "--terminal-live");
    let entities = std::env::args().any(|arg| arg == "--entity-live");
    if entities {
        studio_runtime_services::entity_publication::install(&store)
            .await
            .expect("entity publisher");
        store.transaction_with(|tx| {
            for index in 0..=studio_runtime_schema::shared_values::SYNC_ENTITY_STREAM_MAX_DOCUMENTS {
                let id = format!("older-{index}");
                SyncDocumentsRepository::put_entity(tx, "agentSummary", &id, serde_json::json!({"id":id,"status":"idle","source":"managed"}), false)?;
            }
            AgentRepository::put(tx, &AgentRecord::from_value(serde_json::json!({"id":"lead","rootId":"lead","epoch":1,"status":"idle","name":"Lead"}))?)?;
            Ok::<_, StoreError>(())
        }).await.expect("entity baseline");
    }
    if std::env::args().any(|argument| argument == "--entity-transfer-work") {
        store.transaction_with(|tx| {
            let results = (0..515).map(|index| serde_json::json!({"id":format!("result-{index}"),"agent":"lead","text":if index == 0 { "界".repeat(studio_runtime_schema::shared_values::SYNC_STREAM_EVENT_MAX_BYTES / 2) } else {format!("Evidence-{index}")},"checks":"fixture","revision":"fixture","files":[],"created":1})).collect::<Vec<_>>();
            let work = serde_json::json!({"id":"transfer-assignment","rootId":"lead","title":"Large existing review","description":"Preserve all review history","status":"running","owner":null,"dependencies":[],"created":1,"updated":1,"version":1,"gate":null,"results":results,"decisions":[],"releases":[]});
            tx.execute("INSERT INTO runtime_work(id,record) VALUES(?1,?2)",rusqlite::params!["transfer-assignment",work.to_string()])?;
            Ok::<_, StoreError>(())
        }).await.expect("large existing work source");
    }
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
        Some(store.clone()),
        None,
    );
    let mut app = router(state).expect("real router");
    if terminals {
        app = app.merge(
            axum::Router::new()
                .route("/fixture/terminal", axum::routing::post(write_terminal))
                .with_state(store.clone()),
        );
    }
    if entities {
        app = app.merge(
            axum::Router::new()
                .route("/fixture/entity", axum::routing::post(write_entity))
                .with_state(store),
        );
    }
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
