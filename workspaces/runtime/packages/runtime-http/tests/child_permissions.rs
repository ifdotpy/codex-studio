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
    std::fs::write(&registry, json!({"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":cwd,"label":"Fixture","source":"Fixture","provider":"claude","status":"signedOut"}},"logins":{},"deleteReceipts":{}}).to_string()).unwrap();
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
            .read_until_ready("default", &cwd, "claude", false)
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
        let parent = json!({"id":LEAD,"rootId":LEAD,"epoch":0,"status":"idle","isLead":true,"name":"Lead","cwd":cwd,"hostProjectPath":cwd,"accountKey":"default","provider":"claude","model":"claude-haiku-4-5","autoWake":true,"yoloMode":true,"workspaceMode":"worktree","workerDefaults":{"model":"claude-haiku-4-5"},"claudeOptions":{"permissionMode":mode,"thinking":false}});
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
