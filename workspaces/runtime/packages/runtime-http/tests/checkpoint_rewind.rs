//! Real Git trees through the HTTP preview and restore caller.
#![allow(clippy::unwrap_used, clippy::disallowed_methods)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::account_session_runtime::*;
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore};
use tower::ServiceExt;

#[derive(Default)]
struct Provider {
    calls: AtomicUsize,
    block: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl AccountSessionPort for Provider {
    fn ensure_session<'a>(&'a self, account: &'a str) -> AccountSessionFuture<'a, ConnectionScope> {
        Box::pin(async move {
            Ok(ConnectionScope {
                account: account.into(),
                connection: "fixture".into(),
                generation: 1,
            })
        })
    }
    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.block.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(SessionReceipt {
                scope: request.expected_scope.unwrap(),
                operation: request.operation,
                ticket: None,
                certainty: RequestCertainty::Written,
                result: Some(ProviderResponse::Reply(json!({"thread":{"id":"restored"}}))),
                not_sent_reason: None,
                replayed: false,
            })
        })
    }
}
fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
async fn post(app: &axum::Router, path: &str, body: Value) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("host", "127.0.0.1:49171")
                .header("content-type", "application/json")
                .header("x-canvas-token", "fixture-token")
                .header(
                    studio_runtime_http::API_SCHEMA_HASH_HEADER,
                    studio_runtime_http::schema::api_schema_hash(),
                )
                .extension(ConnectInfo(
                    "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn update_agent(store: &RuntimeStore, key: &'static str, value: Value) {
    store
        .transaction(move |tx| {
            let mut agent = AgentRepository::get(tx, &AgentId("worker".into()))?
                .unwrap()
                .value();
            agent[key] = value;
            AgentRepository::put(tx, &AgentRecord::from_value(agent).unwrap())?;
            Ok(())
        })
        .await
        .unwrap();
}
async fn fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Arc<RuntimeStore>,
    axum::Router,
    Arc<Provider>,
) {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let cwd = root.path().join("worker");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("file.txt"), "checkpoint\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "Initial"]);
    git(
        &repo,
        &["worktree", "add", "-qb", "worker", cwd.to_str().unwrap()],
    );
    std::fs::create_dir(root.path().join("state")).unwrap();
    let store = Arc::new(
        RuntimeStore::open(Some(&root.path().join("state")))
            .await
            .unwrap(),
    );
    let agent = json!({"id":"worker","rootId":"worker","epoch":1,"cwd":cwd,"status":"paused","inFlight":false,"worktreeReady":true,"threadId":"source","accountKey":"default"});
    store
        .transaction(move |tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(agent.clone()).unwrap())?;
            tx.execute(
                "INSERT INTO sync_entities VALUES('agent','worker',1,'fixture',?1,0)",
                [json!({"collection":"agent","id":"worker","value":agent}).to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let provider = Arc::new(Provider::default());
    let mut state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        root.path().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    );
    state.account_session = Some(provider.clone());
    let app = router(state).unwrap();
    (root, cwd, store, app, provider)
}
#[tokio::test]
async fn checkpoint_rewind_preview_and_restore_enforce_idle_and_preserve_before_tree() {
    let (_root, cwd, store, app, provider) = fixture().await;
    let (status, captured) = post(
        &app,
        "/api/checkpoint",
        json!({"agent":"worker","label":"Saved"}),
    )
    .await;
    assert_eq!(status, 200, "{captured}");
    let checkpoint = captured["id"].as_str().unwrap();
    std::fs::write(cwd.join("file.txt"), "later\n").unwrap();
    std::fs::write(cwd.join("new.txt"), "untracked\n").unwrap();
    let preview_body = json!({"agent":"worker","checkpoint":checkpoint});
    let (status, preview) = post(&app, "/api/checkpoint/preview", preview_body.clone()).await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["canRestore"], true, "{preview}");
    assert!(preview["restoreUnavailableReason"].is_null());
    let restore_body =
        json!({"agent":"worker","checkpoint":checkpoint,"expectedTree":preview["expectedTree"]});
    // A stale preview must not fork a provider thread or overwrite later files.
    std::fs::write(cwd.join("file.txt"), "changed after preview\n").unwrap();
    assert_eq!(
        post(&app, "/api/checkpoint/restore", restore_body.clone())
            .await
            .0,
        409
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    std::fs::write(cwd.join("file.txt"), "later\n").unwrap();
    // A monitor blocks both preview eligibility and the mutation.
    let monitor = json!({"id":"monitor","cwd":cwd,"status":"running"});
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_monitors(id,record) VALUES('monitor',?1)",
                [monitor.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, blocked) = post(&app, "/api/checkpoint/preview", preview_body.clone()).await;
    assert_eq!(blocked["canRestore"], false);
    assert_eq!(
        blocked["restoreUnavailableReason"],
        "A monitor is using this workspace"
    );
    assert_ne!(
        post(&app, "/api/checkpoint/restore", restore_body.clone())
            .await
            .0,
        200
    );
    store
        .transaction(|tx| {
            tx.execute("DELETE FROM runtime_monitors WHERE id='monitor'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    for (key, value) in [("inFlight", json!(true)), ("worktreeReady", json!(false))] {
        update_agent(&store, key, value).await;
        let (status, blocked) = post(&app, "/api/checkpoint/preview", preview_body.clone()).await;
        assert_eq!(status, 200, "{blocked}");
        assert_eq!(blocked["canRestore"], false);
        assert!(
            !blocked["restoreUnavailableReason"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        assert_ne!(
            post(&app, "/api/checkpoint/restore", restore_body.clone())
                .await
                .0,
            200
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        update_agent(&store, key, json!(key == "worktreeReady")).await;
    }
    let (status, restored) = post(&app, "/api/checkpoint/restore", restore_body.clone()).await;
    assert_eq!(status, 200, "{restored}");
    assert_eq!(
        std::fs::read_to_string(cwd.join("file.txt")).unwrap(),
        "checkpoint\n"
    );
    assert!(!cwd.join("new.txt").exists());
    let db = store.test_reader().unwrap();
    let before: String = db.query_row("SELECT json_extract(record,'$.tree') FROM runtime_checkpoints WHERE json_extract(record,'$.label')='Before restore'",[],|row|row.get(0)).unwrap();
    assert_eq!(json!(before), preview["expectedTree"]);
    let agent: String = db
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='worker'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let agent: Value = serde_json::from_str(&agent).unwrap();
    assert_eq!(agent["threadId"], "restored");
    assert_eq!(agent["status"], "paused");
    let projected: String = db
        .query_row(
            "SELECT payload FROM sync_entities WHERE collection='agent' AND id='worker'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let projected: Value = serde_json::from_str(&projected).unwrap();
    assert_eq!(projected["value"]["threadId"], "restored");
    assert!(projected["value"]["workspaceOperation"].is_null());
    assert_eq!(
        post(&app, "/api/checkpoint/restore", restore_body).await.0,
        200
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

async fn pending_restore(app: &axum::Router, cwd: &Path) -> (Value, Value) {
    let (status, captured) = post(
        app,
        "/api/checkpoint",
        json!({"agent":"worker","label":"Saved"}),
    )
    .await;
    assert_eq!(status, 200, "{captured}");
    std::fs::write(cwd.join("file.txt"), "later\n").unwrap();
    let preview_body = json!({"agent":"worker","checkpoint":captured["id"]});
    let (status, preview) = post(app, "/api/checkpoint/preview", preview_body.clone()).await;
    assert_eq!(status, 200, "{preview}");
    (
        preview_body,
        json!({"agent":"worker","checkpoint":captured["id"],"expectedTree":preview["expectedTree"]}),
    )
}

#[tokio::test]
async fn overlapping_monitor_and_child_reservation_block_restore() {
    let (_root, cwd, store, app, provider) = fixture().await;
    let (preview, restore) = pending_restore(&app, &cwd).await;
    let child = cwd.join("nested");
    std::fs::create_dir(&child).unwrap();
    for kind in ["monitor", "agent", "reservation"] {
        let child = child.clone();
        store.transaction(move |tx| {
            if kind == "monitor" {
                tx.execute("INSERT INTO runtime_monitors VALUES('child',?1)",[json!({"id":"child","cwd":child,"status":"running"}).to_string()])?;
            } else {
                let agent=json!({"id":"child","rootId":"worker","epoch":1,"status":"paused","cwd":child,"inFlight":kind=="agent","workspaceOperation":if kind=="reservation" {json!("restore")} else {Value::Null}});
                AgentRepository::put(tx,&AgentRecord::from_value(agent).unwrap())?;
            }
            Ok(())
        }).await.unwrap();
        let (_, blocked) = post(&app, "/api/checkpoint/preview", preview.clone()).await;
        assert_eq!(blocked["canRestore"], false, "{kind}: {blocked}");
        assert!(
            blocked["restoreUnavailableReason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty())
        );
        assert_ne!(
            post(&app, "/api/checkpoint/restore", restore.clone())
                .await
                .0,
            200
        );
        store
            .transaction(|tx| {
                tx.execute("DELETE FROM runtime_agents WHERE id='child'", [])?;
                tx.execute("DELETE FROM runtime_monitors WHERE id='child'", [])?;
                Ok(())
            })
            .await
            .unwrap();
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn restore_recovery_reestablishes_owned_capture_after_transient_git_failure() {
    let (root, cwd, store, app, _provider) = fixture().await;
    let (preview, restore) = pending_restore(&app, &cwd).await;
    let op = format!(
        "restore:worker:{}:{}",
        restore["checkpoint"].as_str().unwrap(),
        &studio_runtime_store::sha256_hex(restore["expectedTree"].as_str().unwrap().as_bytes())
            [..32]
    );
    let checkpoint = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, op.as_bytes());
    let lock = root.path().join(format!(
        "repo/.git/refs/codex-agents/checkpoints/worker/{checkpoint}.lock"
    ));
    std::fs::write(&lock, "fixture lock").unwrap();
    let (status, result) = post(&app, "/api/checkpoint/restore", restore.clone()).await;
    assert_ne!(status, 200, "{result}");
    let agent = AgentRepository::get(&store.test_reader().unwrap(), &AgentId("worker".into()))
        .unwrap()
        .unwrap()
        .value();
    assert_eq!(agent["workspaceOperation"], "restore_recovery");
    assert_eq!(
        std::fs::read_to_string(cwd.join("file.txt")).unwrap(),
        "later\n"
    );
    std::fs::remove_file(lock).unwrap();
    let (_, available) = post(&app, "/api/checkpoint/preview", preview).await;
    assert_eq!(available["canRestore"], true, "{available}");
    let (status, result) = post(&app, "/api/checkpoint/restore", restore).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(
        std::fs::read_to_string(cwd.join("file.txt")).unwrap(),
        "checkpoint\n"
    );
    let count:i64=store.test_reader().unwrap().query_row("SELECT count(*) FROM runtime_checkpoints WHERE json_extract(record,'$.label')='Before restore'",[],|row|row.get(0)).unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn concurrent_restore_retry_cannot_execute_or_demote_completed_operation() {
    let (_root, cwd, store, app, provider) = fixture().await;
    let (_, restore) = pending_restore(&app, &cwd).await;
    provider.block.store(true, Ordering::SeqCst);
    let first_app = app.clone();
    let first_body = restore.clone();
    let first =
        tokio::spawn(async move { post(&first_app, "/api/checkpoint/restore", first_body).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.entered.notified(),
    )
    .await
    .unwrap();
    assert_ne!(
        post(&app, "/api/checkpoint/restore", restore.clone())
            .await
            .0,
        200
    );
    provider.release.notify_one();
    let (status, result) = first.await.unwrap();
    assert_eq!(status, 200, "{result}");
    let (status, result) = post(&app, "/api/checkpoint/restore", restore).await;
    assert_eq!(status, 200, "{result}");
    let phase:String=store.test_reader().unwrap().query_row("SELECT json_extract(record,'$.phase') FROM runtime_workspace_operations WHERE json_extract(record,'$.kind')='restore'",[],|row|row.get(0)).unwrap();
    assert_eq!(phase, "completed");
    let agent = AgentRepository::get(&store.test_reader().unwrap(), &AgentId("worker".into()))
        .unwrap()
        .unwrap()
        .value();
    assert!(agent["workspaceOperation"].is_null());
    assert!(agent["workspaceReservationId"].is_null());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalid_checkpoint_tree_refuses_before_provider_or_file_effects() {
    let (_root, cwd, store, app, provider) = fixture().await;
    let (_, restore) = pending_restore(&app, &cwd).await;
    let checkpoint = restore["checkpoint"].as_str().unwrap().to_owned();
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_checkpoints SET record=json_set(record,'$.tree',?1) WHERE id=?2",
                ["0000000000000000000000000000000000000000", &checkpoint],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_ne!(post(&app, "/api/checkpoint/restore", restore).await.0, 200);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read_to_string(cwd.join("file.txt")).unwrap(),
        "later\n"
    );
}
