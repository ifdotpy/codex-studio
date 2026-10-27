//! The UI's HTTP caller inherits the current Claude permissions before commit.
#![allow(clippy::unwrap_used, clippy::disallowed_methods)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::{account_driver::AccountDriver, account_session_runtime::*};
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore};
use tower::ServiceExt;

const LEAD: &str = "bf2eeb40-0cc8-44ef-a4e8-aa321b4d7331";
struct Catalog;
impl AccountSessionPort for Catalog {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        Some(ConnectionScope {
            account: account.into(),
            connection: "fixture".into(),
            generation: 1,
        })
    }
    fn provider_request(
        &self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'_, SessionReceipt> {
        assert_eq!(request.method, studio_runtime_providers::Method::ModelList);
        Box::pin(async move {
            Ok(SessionReceipt {
                scope: request.expected_scope.unwrap(),
                operation: request.operation,
                ticket: Some(studio_runtime_providers::RequestTicket {
                    generation: 1,
                    id: studio_runtime_providers::RequestId::String("catalog".into()),
                    method: request.method,
                }),
                certainty: RequestCertainty::Written,
                result: Some(ProviderResponse::Reply(
                    json!({"data":[{"model":"claude-haiku-4-5","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}],"nextCursor":null}),
                )),
                not_sent_reason: None,
                replayed: false,
            })
        })
    }
}
async fn call(app: &axum::Router, body: Value) -> (u16, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/agents")
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", "fixture-token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn ui_child_permissions_inherit_override_review_and_refuse_escalation() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@localhost",
            "commit",
            "--allow-empty",
            "-qm",
            "Fixture",
        ],
    ] {
        let result = std::process::Command::new("git")
            .current_dir(root.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let state_root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(state_root.path())).await.unwrap());
    let registry = root.path().join("accounts.json");
    let missing_cli = root.path().join("missing-claude-cli");
    std::fs::write(&registry, json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":cwd,"label":"Fixture","source":"Fixture","provider":"claude","status":"signedOut","claudeOptions":{"binaryPath":missing_cli}}},"logins":{},"deleteReceipts":{}}).to_string()).unwrap();
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into_owned(),
        49171,
        None,
        Some(store.clone()),
        None,
    )
    .with_account_driver(Arc::new(AccountDriver::open(&registry).unwrap()));
    let catalog = state.model_catalog_handle();
    catalog.set_account_session(Some(Arc::new(Catalog)));
    assert!(matches!(
        catalog
            .read_until_ready(
                "default",
                &cwd,
                studio_runtime_providers::ProviderKind::Claude,
                false
            )
            .await,
        studio_runtime_services::model_catalog::CatalogRead::Ready(_)
    ));
    let app = router(state).unwrap();
    for (mode, role, requested, expected) in [
        ("auto", "implementer", None, "auto"),
        ("plan", "implementer", None, "plan"),
        ("acceptEdits", "implementer", None, "acceptEdits"),
        (
            "bypassPermissions",
            "reviewer",
            Some("bypassPermissions"),
            "plan",
        ),
        ("auto", "implementer", Some("acceptEdits"), "acceptEdits"),
        ("auto", "implementer", Some("bypassPermissions"), "auto"),
        ("plan", "implementer", Some("bypassPermissions"), "plan"),
        (
            "acceptEdits",
            "implementer",
            Some("bypassPermissions"),
            "acceptEdits",
        ),
        (
            "bypassPermissions",
            "implementer",
            Some("bypassPermissions"),
            "bypassPermissions",
        ),
    ] {
        let parent = json!({"id":LEAD,"rootId":LEAD,"epoch":0,"status":"idle","isLead":true,"name":"Lead","cwd":cwd,"hostProjectPath":cwd,"accountKey":"default","provider":"claude","model":"claude-haiku-4-5","autoWake":true,"yoloMode":true,"workspaceMode":"worktree","workerDefaults":{"accountKey":"default","model":"claude-haiku-4-5"},"claudeOptions":{"permissionMode":mode,"thinking":false}});
        let projected =
            studio_runtime_services::sync_views::project_persisted_record("agent", &parent)
                .unwrap();
        assert_eq!(projected["claudeOptions"], json!({"permissionMode":mode}));
        store
            .transaction(move |tx| {
                AgentRepository::put(tx, &AgentRecord::from_value(parent).unwrap())
            })
            .await
            .unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let mut body =
            json!({"id":id,"parent":LEAD,"name":"Child","prompt":"Read the task","role":role});
        if let Some(mode) = requested {
            body["permission_mode"] = json!(mode);
        }
        let (status, response) = call(&app, body.clone()).await;
        assert_eq!(status, 200, "{mode} {role} {requested:?}: {response}");
        let child =
            AgentRepository::get(&store.test_reader().unwrap(), &AgentId::from(id.as_str()))
                .unwrap()
                .unwrap()
                .value();
        assert_eq!(child["claudeOptions"], json!({"permissionMode":expected}));
        assert_eq!(child["yoloMode"], expected == "bypassPermissions");
        let (status, _) = call(&app, body).await;
        assert_eq!(status, 200, "The accepted request remains replayable");
    }
    let (status, _) = call(
        &app,
        json!({"parent":LEAD,"name":"Invalid","prompt":"Read","permission_mode":"unknown"}),
    )
    .await;
    assert!((400..500).contains(&status));
    catalog.shutdown().await.unwrap();
}

struct RoutingCatalog;
impl AccountSessionPort for RoutingCatalog {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        Some(ConnectionScope {
            account: account.into(),
            connection: "fixture".into(),
            generation: 1,
        })
    }
    fn provider_request(
        &self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'_, SessionReceipt> {
        Box::pin(async move {
            Ok(SessionReceipt {
                scope: request.expected_scope.unwrap(),
                operation: request.operation,
                ticket: Some(studio_runtime_providers::RequestTicket {
                    generation: 1,
                    id: studio_runtime_providers::RequestId::String("catalog".into()),
                    method: request.method,
                }),
                certainty: RequestCertainty::Written,
                result: Some(ProviderResponse::Reply(
                    json!({"data":[{"model":"gpt-6-luna","supportedReasoningEfforts":[{"reasoningEffort":"high"}]}],"nextCursor":null}),
                )),
                not_sent_reason: None,
                replayed: false,
            })
        })
    }
}

#[tokio::test]
async fn automatic_http_child_warms_higher_priority_catalog_before_selection() {
    let root = tempfile::tempdir().unwrap();
    let state_root = tempfile::tempdir().unwrap();
    let cwd = root
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let parent_home = root.path().join("parent-home");
    let priority_home = root.path().join("priority-home");
    let claude_home = root.path().join("claude-home");
    let missing_cli = root.path().join("missing-claude-cli");
    std::fs::create_dir_all(&parent_home).unwrap();
    std::fs::create_dir_all(&priority_home).unwrap();
    std::fs::create_dir_all(&claude_home).unwrap();
    std::fs::write(
        parent_home.join("auth.json"),
        r#"{"OPENAI_API_KEY":"fixture-parent"}"#,
    )
    .unwrap();
    std::fs::write(
        priority_home.join("auth.json"),
        r#"{"OPENAI_API_KEY":"fixture-priority"}"#,
    )
    .unwrap();
    let registry = root.path().join("accounts.json");
    std::fs::write(&registry, json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,
        "defaultAccountKey":"default","accounts":{
            "default":{"id":"default","home":parent_home,"label":"Parent","source":"Fixture","provider":"codex","status":"ready"},
            "a-priority":{"id":"a-priority","home":priority_home,"label":"Priority","source":"Fixture","provider":"codex","status":"ready"},
            "claude-parent":{"id":"claude-parent","home":claude_home,"label":"Claude","source":"Fixture","provider":"claude","status":"signedOut","claudeOptions":{"binaryPath":missing_cli}}},
        "logins":{},"deleteReceipts":{}}).to_string()).unwrap();
    let store = Arc::new(RuntimeStore::open(Some(state_root.path())).await.unwrap());
    let lead = json!({"id":LEAD,"rootId":LEAD,"epoch":0,"status":"idle","isLead":true,
        "name":"Lead","cwd":cwd,"hostProjectPath":cwd,"accountKey":"default","provider":"codex",
        "model":"gpt-6-luna","autoWake":true,"yoloMode":true,"workspaceMode":"shared",
        "concurrency":4,"maxAgents":20,"workerDefaults":{"model":"gpt-6-luna"}});
    store
        .transaction(move |tx| AgentRepository::put(tx, &AgentRecord::from_value(lead).unwrap()))
        .await
        .unwrap();
    let mut state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into_owned(),
        49171,
        None,
        Some(store.clone()),
        None,
    )
    .with_account_driver(Arc::new(AccountDriver::open(&registry).unwrap()));
    let session = Arc::new(RoutingCatalog);
    let catalog = state.model_catalog_handle();
    catalog.set_account_session(Some(session.clone()));
    state.account_session = Some(session);
    assert!(matches!(
        catalog
            .read_until_ready(
                "default",
                parent_home.to_str().unwrap(),
                studio_runtime_providers::ProviderKind::Codex,
                false
            )
            .await,
        studio_runtime_services::model_catalog::CatalogRead::Ready(_)
    ));
    let app = router(state).unwrap();
    let body = json!({"id":"ec2d6635-e504-4d2e-a5ed-a4631393e2cc","parent":LEAD,"name":"Child","prompt":"Read","role":"implementer"});
    let (status, response) = call(&app, body.clone()).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["accountKey"], "a-priority");
    let claude_lead_id = "af2eeb40-0cc8-44ef-a4e8-aa321b4d7331";
    let claude_lead = json!({"id":claude_lead_id,"rootId":claude_lead_id,"epoch":0,"status":"idle","isLead":true,
        "name":"Claude lead","cwd":cwd,"hostProjectPath":cwd,"accountKey":"claude-parent","provider":"claude",
        "model":"claude-haiku-4-5","autoWake":true,"yoloMode":true,"workspaceMode":"shared",
        "concurrency":4,"maxAgents":20,"workerDefaults":{}});
    store.transaction(move |tx| {
        AgentRepository::put(tx, &AgentRecord::from_value(claude_lead).unwrap())?;
        tx.execute("INSERT INTO runtime_profiles(id,record) VALUES(?1,?2)", rusqlite::params!["codex-profile", json!({"id":"codex-profile","name":"Codex profile","role":"implementer","model":"gpt-6-luna","effort":null,"instructions":""}).to_string()])?;
        Ok(())
    }).await.unwrap();
    let (profile_status, profile_child) = call(&app, json!({"id":"bc2d6635-e504-4d2e-a5ed-a4631393e2cc","parent":claude_lead_id,"profile_id":"codex-profile","name":"Profile child","prompt":"Read"})).await;
    assert_eq!(profile_status, 200, "{profile_child}");
    assert_eq!(profile_child["provider"], "codex");
    assert_eq!(profile_child["model"], "gpt-6-luna");
    catalog.shutdown().await.unwrap();
    store
        .transaction(|tx| {
            tx.execute("DELETE FROM runtime_agents WHERE id=?1", [LEAD])?;
            Ok(())
        })
        .await
        .unwrap();
    let mut fresh = body.clone();
    fresh["id"] = json!("dc2d6635-e504-4d2e-a5ed-a4631393e2cc");
    let (fresh_status, _) = call(&app, fresh).await;
    assert_eq!(
        fresh_status, 500,
        "new admission must fail without its parent"
    );
    let (status, replay) = call(&app, body).await;
    assert_eq!(
        status, 200,
        "replay must precede parent and catalog admission: {replay}"
    );
    assert_eq!(replay["accountKey"], "a-priority");
}
