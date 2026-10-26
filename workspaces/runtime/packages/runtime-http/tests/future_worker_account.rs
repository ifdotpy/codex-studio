//! The future-worker picker waits for fresh GET metadata before its settings POST.
#![allow(clippy::disallowed_methods)]

use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use studio_runtime_http::{HttpState, router};
use studio_runtime_providers::{Method, RequestId, RequestTicket};
use studio_runtime_services::{
    account_driver::AccountDriver,
    account_policy::AccountPolicyState,
    account_session_runtime::{
        AccountSessionError, AccountSessionFuture, AccountSessionPort, ConnectionScope,
        ProviderRequest, ProviderResponse, RequestCertainty, SessionReceipt,
    },
    model_catalog::ModelCatalogService,
};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError};
use tokio::sync::{Semaphore, oneshot};
use tower::ServiceExt;

const LEAD: &str = "2edc9402-ef2f-42a3-a2a4-38a8a4f5a3e1";
const CHILD: &str = "6954cfc8-9e99-498a-b994-29fbf43904d3";
const LEAD_ACCOUNT: &str = "pm-claude";
const TARGET: &str = "future-codex";
const OLD: &str = "old-codex";
const MODEL: &str = "gpt-6.1-sol";

struct CatalogSession {
    failed: bool,
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: Semaphore,
    reads: AtomicUsize,
}

impl AccountSessionPort for CatalogSession {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        Some(ConnectionScope {
            account: account.into(),
            connection: "metadata-fixture".into(),
            generation: 7,
        })
    }
    fn ensure_session<'a>(&'a self, account: &'a str) -> AccountSessionFuture<'a, ConnectionScope> {
        Box::pin(async move {
            Ok(self
                .current_scope(account)
                .expect("existing fixture session"))
        })
    }
    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            assert_eq!(
                request.method,
                Method::ModelList,
                "no model turn or account command"
            );
            assert!(request.actor.is_none());
            let scope = self.current_scope(&request.account).expect("scope");
            assert_eq!(request.expected_scope.as_ref(), Some(&scope));
            let read = self.reads.fetch_add(1, Ordering::SeqCst);
            if request.account == TARGET {
                if let Some(started) = self.started.lock().expect("start gate").take() {
                    started.send(()).expect("caller observes target read");
                }
                self.release
                    .acquire()
                    .await
                    .expect("metadata gate")
                    .forget();
            }
            if self.failed {
                return Err(AccountSessionError::Unknown);
            }
            let model = match request.account.as_str() {
                TARGET => MODEL,
                OLD => "gpt-6-luna",
                LEAD_ACCOUNT => "default",
                _ => panic!("foreign account metadata request"),
            };
            Ok(SessionReceipt {
                scope,
                operation: request.operation,
                ticket: Some(RequestTicket {
                    generation: 7,
                    id: RequestId::String(format!("metadata-{read}")),
                    method: Method::ModelList,
                }),
                certainty: RequestCertainty::Written,
                result: Some(ProviderResponse::Reply(json!({"data":[{
                    "model":model,"supportedReasoningEfforts":[{"reasoningEffort":"medium"}],
                    "defaultReasoningEffort":"medium","serviceTiers":[]
                }],"nextCursor":null}))),
                not_sent_reason: None,
                replayed: false,
            })
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    app: axum::Router,
    store: Arc<RuntimeStore>,
    catalog: ModelCatalogService,
    session: Arc<CatalogSession>,
    started: oneshot::Receiver<()>,
}

impl Fixture {
    async fn new(failed: bool) -> Self {
        let root = tempfile::tempdir().expect("isolated PM state");
        let cwd =
            studio_runtime_store::path_identity::resolve(root.path()).expect("canonical fixture");
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("fixture store"),
        );
        let target_home = cwd.join("target");
        let old_home = cwd.join("old");
        let lead_home = cwd.join("lead");
        for (home, key) in [
            (&target_home, "target-fixture-key"),
            (&old_home, "old-fixture-key"),
        ] {
            std::fs::create_dir_all(home).expect("fixture profile");
            std::fs::write(
                home.join("auth.json"),
                json!({"OPENAI_API_KEY":key}).to_string(),
            )
            .expect("synthetic metadata");
        }
        std::fs::create_dir_all(&lead_home).expect("fixture Claude profile");
        let registry = json!({"version":1,"defaultAccountKey":TARGET,"accounts":{
            LEAD_ACCOUNT:{"id":LEAD_ACCOUNT,"home":lead_home,"provider":"claude","label":"PM lead","source":"Fixture","status":"signedOut",
                "claudeOptions":{"configDir":lead_home,"binaryPath":cwd.join("no-provider-executable")}},
            TARGET:{"id":TARGET,"home":target_home,"provider":"codex","label":"Future","source":"Fixture","status":"ready"},
            OLD:{"id":OLD,"home":old_home,"provider":"codex","label":"Existing","source":"Fixture","status":"ready"}
        },"logins":{},"deleteReceipts":{}});
        let registry_bytes = serde_json::to_vec(&registry).expect("registry bytes");
        let registry_path = cwd.join("accounts.json");
        std::fs::write(&registry_path, &registry_bytes).expect("fixture registry");
        let driver = Arc::new(AccountDriver::open(&registry_path).expect("real account driver"));
        let policy = Arc::new(
            AccountPolicyState::from_startup(Some(&registry_bytes), []).expect("account policy"),
        );
        let cwd_text = cwd.to_string_lossy().into_owned();
        store.transaction(move |tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(json!({
                "id":LEAD,"rootId":LEAD,"epoch":3,"name":"PM installation","isLead":true,
                "accountKey":LEAD_ACCOUNT,"provider":"claude","model":"default","status":"waiting",
                "cwd":cwd_text,"autoWake":false,"workerDefaults":{
                    "accountKey":OLD,"model":MODEL,"effort":"medium","fastMode":false,
                    "daybreakEnabled":false,"cyberAccessProgram":"standard","delegationDepth":1
                }
            }))?)?;
            AgentRepository::put(tx, &AgentRecord::from_value(json!({
                "id":CHILD,"parentId":LEAD,"rootId":LEAD,"epoch":2,"name":"Existing worker",
                "accountKey":OLD,"provider":"codex","model":MODEL,"status":"paused",
                "cwd":cwd_text,"autoWake":false,"threadId":"existing-thread"
            }))?)?;
            Ok::<_, StoreError>(())
        }).await.expect("existing PM and worker");
        let (started_tx, started) = oneshot::channel();
        let session = Arc::new(CatalogSession {
            failed,
            started: Mutex::new(Some(started_tx)),
            release: Semaphore::new(0),
            reads: AtomicUsize::new(0),
        });
        let mut state = HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            cwd.to_string_lossy().into_owned(),
            49171,
            None,
            Some(store.clone()),
            Some(policy),
        )
        .with_account_driver(driver);
        state.account_session = Some(session.clone());
        let catalog = state.model_catalog_handle();
        catalog.set_account_session(Some(session.clone()));
        Self {
            _root: root,
            app: router(state).expect("real HTTP routes"),
            store,
            catalog,
            session,
            started,
        }
    }
    fn selection_uri(automatic: bool) -> String {
        if automatic {
            format!("/api/models?account_key={LEAD_ACCOUNT}&workers=1&fresh=1")
        } else {
            format!("/api/models?account_key={TARGET}&fresh=1")
        }
    }
    fn settings(automatic: bool) -> Value {
        json!({"id":LEAD,"expected_account_key":LEAD_ACCOUNT,"worker_defaults":{
            "account_key":if automatic { Value::Null } else { json!(TARGET) },
            "model":MODEL,"effort":"medium","fast_mode":false,"daybreak_enabled":false,"delegation_depth":1
        }})
    }
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", "fixture-token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345"
                .parse::<std::net::SocketAddr>()
                .expect("address"),
        ))
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .expect("UI request");
    let response = app.clone().oneshot(request).await.expect("HTTP response");
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("response body");
    (
        status,
        serde_json::from_slice(&bytes).expect("response JSON"),
    )
}

fn actor(store: &RuntimeStore, id: &str) -> Value {
    let raw: String = store
        .test_reader()
        .expect("fixture reader")
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .expect("fixture actor");
    serde_json::from_str(&raw).expect("actor JSON")
}

#[tokio::test]
async fn fresh_get_then_real_future_worker_settings_post_preserves_existing_workers() {
    for automatic in [false, true] {
        let fixture = Fixture::new(false).await;
        let before = actor(&fixture.store, LEAD);
        let child = actor(&fixture.store, CHILD);
        let app = fixture.app.clone();
        let uri = Fixture::selection_uri(automatic);
        let pending = tokio::spawn(async move { call(&app, "GET", &uri, None).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), fixture.started)
            .await
            .expect("metadata starts")
            .expect("target read");
        assert!(
            !pending.is_finished(),
            "the exact target metadata is still gated"
        );
        assert_eq!(actor(&fixture.store, LEAD), before);
        assert_eq!(actor(&fixture.store, CHILD), child);
        fixture.session.release.add_permits(1);
        let (status, models) = pending.await.expect("GET task");
        assert_eq!(status, 200, "fresh account metadata: {models}");
        assert!(
            models["data"]
                .as_array()
                .expect("offered models")
                .iter()
                .any(|row| row["model"] == MODEL)
        );

        let (status, saved) = call(
            &fixture.app,
            "POST",
            "/api/conversation",
            Some(Fixture::settings(automatic)),
        )
        .await;
        assert_eq!(
            status, 200,
            "future-worker settings after fresh GET: {saved}"
        );
        assert_eq!(
            saved["workerDefaults"]["accountKey"],
            if automatic {
                Value::Null
            } else {
                json!(TARGET)
            }
        );
        assert_eq!(saved["workerDefaults"]["model"], MODEL);
        let after = actor(&fixture.store, LEAD);
        for key in [
            "id",
            "epoch",
            "accountKey",
            "provider",
            "status",
            "autoWake",
            "model",
        ] {
            assert_eq!(after[key], before[key], "lead identity and hold: {key}");
        }
        assert_eq!(actor(&fixture.store, CHILD), child);
        fixture.catalog.shutdown().await.expect("catalog shutdown");
    }
}

#[tokio::test]
async fn failed_fresh_metadata_cannot_change_future_defaults_or_existing_workers() {
    for automatic in [false, true] {
        let fixture = Fixture::new(true).await;
        let before = actor(&fixture.store, LEAD);
        let child = actor(&fixture.store, CHILD);
        let app = fixture.app.clone();
        let uri = Fixture::selection_uri(automatic);
        let pending = tokio::spawn(async move { call(&app, "GET", &uri, None).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), fixture.started)
            .await
            .expect("metadata starts")
            .expect("target read");
        fixture.session.release.add_permits(1);
        let (status, error) = pending.await.expect("GET task");
        assert_ne!(
            status, 200,
            "a failed metadata read cannot authorize a POST: {error}"
        );
        assert!(error["error"].is_string());
        assert_eq!(actor(&fixture.store, LEAD), before);
        assert_eq!(actor(&fixture.store, CHILD), child);
        fixture.catalog.shutdown().await.expect("catalog shutdown");
    }
}
