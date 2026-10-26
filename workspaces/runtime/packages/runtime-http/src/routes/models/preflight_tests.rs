//! The UI waits for an exact metadata receipt, not complete but stale model rows.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use studio_runtime_providers::{Method, RequestId, RequestTicket};
use studio_runtime_schema::shared_values::{
    MODEL_CATALOG_ADMISSION_WAIT_SECONDS, MODEL_CATALOG_CACHE_TTL_SECONDS,
};
use studio_runtime_services::account_driver::AccountDriver;
use studio_runtime_services::account_policy::AccountPolicyState;
use studio_runtime_services::account_session_runtime::{
    AccountSessionError, AccountSessionFuture, AccountSessionPort, ConnectionScope,
    ProviderRequest, ProviderResponse, RequestCertainty, SessionReceipt,
};
use studio_runtime_services::model_catalog::CatalogRead;
use studio_runtime_services::resource_notify::ResourceChanges;
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError};
use tokio::sync::{Notify, Semaphore, oneshot};
use tower::ServiceExt;

use crate::router::HttpState;

const LEAD: &str = "catalog-preflight-lead";
const CHILD: &str = "catalog-preflight-existing-child";
const LEAD_ACCOUNT: &str = "lead-claude";
const TARGET: &str = "target-codex";
const REQUEST_ID: &str = "future-worker-catalog-one";
const MODEL: &str = "gpt-6.1-sol";

#[derive(Clone, Copy)]
enum RefreshReply {
    SameContent,
    Failed,
    ForeignScope,
}

struct CatalogSession {
    reads: AtomicUsize,
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: Semaphore,
    reply: RefreshReply,
}

impl AccountSessionPort for CatalogSession {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        Some(ConnectionScope {
            account: account.to_owned(),
            connection: "catalog-preflight-connection".into(),
            generation: 7,
        })
    }

    fn ensure_session<'a>(&'a self, account: &'a str) -> AccountSessionFuture<'a, ConnectionScope> {
        Box::pin(async move { Ok(self.current_scope(account).expect("fixture scope")) })
    }

    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            assert_eq!(
                request.method,
                Method::ModelList,
                "metadata only, never an LLM turn"
            );
            assert_eq!(
                request.account, TARGET,
                "the explicit target owns this read"
            );
            assert!(request.actor.is_none());
            let mut scope = self.current_scope(TARGET).expect("target scope");
            assert_eq!(request.expected_scope.as_ref(), Some(&scope));
            let read = self.reads.fetch_add(1, Ordering::SeqCst);
            if read > 0 {
                if let Some(started) = self.started.lock().expect("refresh sender").take() {
                    started.send(()).expect("refresh observed");
                }
                self.release
                    .acquire()
                    .await
                    .expect("owned refresh gate")
                    .forget();
                match self.reply {
                    RefreshReply::Failed => return Err(AccountSessionError::Unknown),
                    RefreshReply::ForeignScope => scope.account = "another-account".into(),
                    RefreshReply::SameContent => {}
                }
            }
            Ok(SessionReceipt {
                scope,
                operation: request.operation,
                ticket: Some(RequestTicket {
                    id: RequestId::String(format!("metadata-{read}")),
                    generation: 7,
                    method: Method::ModelList,
                }),
                certainty: RequestCertainty::Written,
                result: Some(ProviderResponse::Reply(json!({
                    "data":[{
                        "model":MODEL,
                        "supportedReasoningEfforts":[{"reasoningEffort":"medium"}],
                        "defaultReasoningEffort":"medium"
                    }],
                    "nextCursor":null
                }))),
                not_sent_reason: None,
                replayed: false,
            })
        })
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    state: HttpState,
    app: axum::Router,
    store: Arc<RuntimeStore>,
    session: Arc<CatalogSession>,
    started: oneshot::Receiver<()>,
}

impl Fixture {
    async fn new(reply: RefreshReply) -> Self {
        let directory = tempfile::tempdir().expect("isolated metadata fixture");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("fixture store"),
        );
        let target_home = directory.path().join("target-profile");
        let old_home = directory.path().join("old-profile");
        let lead_home = directory.path().join("lead-profile");
        for (home, key) in [
            (&target_home, "target-test-key"),
            (&old_home, "old-test-key"),
        ] {
            std::fs::create_dir_all(home).expect("fixture profile");
            std::fs::write(
                home.join("auth.json"),
                json!({"OPENAI_API_KEY":key}).to_string(),
            )
            .expect("fixture auth metadata");
        }
        std::fs::create_dir_all(&lead_home).expect("isolated lead profile");
        let registry = json!({
            "version":1,"defaultAccountKey":TARGET,"accounts":{
                LEAD_ACCOUNT:{"id":LEAD_ACCOUNT,"home":lead_home,"provider":"claude","label":"Lead","source":"Fixture","status":"signedOut",
                    "claudeOptions":{"configDir":lead_home,"binaryPath":directory.path().join("no-provider-executable")}},
                TARGET:{"id":TARGET,"home":target_home,"provider":"codex","label":"Target","source":"Fixture","status":"ready"},
                "old-codex":{"id":"old-codex","home":old_home,"provider":"codex","label":"Old","source":"Fixture","status":"ready"}
            },"logins":{},"deleteReceipts":{}
        });
        let bytes = serde_json::to_vec(&registry).expect("registry JSON");
        let registry_path = directory.path().join("accounts.json");
        std::fs::write(&registry_path, &bytes).expect("isolated registry");
        let driver = Arc::new(AccountDriver::open(&registry_path).expect("fixture driver"));
        let policy = Arc::new(
            AccountPolicyState::from_startup(Some(&bytes), std::iter::empty::<(String, Value)>())
                .expect("fixture policy"),
        );
        store.transaction_with(|tx| {
            let lead = AgentRecord::from_value(json!({
                "id":LEAD,"rootId":LEAD,"epoch":3,"name":"PM fixture","isLead":true,
                "accountKey":LEAD_ACCOUNT,"provider":"claude","model":"default","status":"waiting",
                "cwd":"/fixture","autoWake":false,"workerDefaults":{
                    "accountKey":"old-codex","model":MODEL,"effort":"medium","fastMode":false,"daybreakEnabled":false,"delegationDepth":1
                }
            }))?;
            let child = AgentRecord::from_value(json!({
                "id":CHILD,"parentId":LEAD,"rootId":LEAD,"epoch":2,"name":"Existing child",
                "accountKey":"old-codex","provider":"codex","model":MODEL,"status":"waiting",
                "cwd":"/fixture","autoWake":false
            }))?;
            AgentRepository::put(tx, &lead)?;
            AgentRepository::put(tx, &child)?;
            Ok::<_, StoreError>(())
        }).await.expect("existing chat and child");
        let (started_tx, started) = oneshot::channel();
        let session = Arc::new(CatalogSession {
            reads: AtomicUsize::new(0),
            started: Mutex::new(Some(started_tx)),
            release: Semaphore::new(0),
            reply,
        });
        let mut state = HttpState::new(
            "fixture-token".into(),
            "fixture-workspace".into(),
            directory.path().display().to_string(),
            4620,
            None,
            Some(store.clone()),
            Some(policy),
        )
        .with_account_driver(driver.clone());
        state.account_session = Some(session.clone());
        let catalog = state.model_catalog_handle();
        catalog.set_account_session(Some(session.clone()));
        catalog.retain_accounts(
            &driver
                .snapshot()
                .expect("saved account identities")
                .accounts,
        );
        let commits = Arc::new(Notify::new());
        let notify = commits.clone();
        store.set_commit_notifier(move || notify.notify_one());
        assert!(matches!(
            catalog
                .read_until_ready(
                    TARGET,
                    target_home.to_str().expect("profile path"),
                    "codex",
                    false
                )
                .await,
            CatalogRead::Ready(_)
        ));
        loop {
            let committed = commits.notified();
            tokio::pin!(committed);
            committed.as_mut().enable();
            if model_row(&store).is_some() {
                break;
            }
            committed.await;
        }
        tokio::time::advance(Duration::from_secs(MODEL_CATALOG_CACHE_TTL_SECONDS + 1)).await;
        let app = crate::router::router(state.clone()).expect("real HTTP routes");
        Self {
            _directory: directory,
            state,
            app,
            store,
            session,
            started,
        }
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("host", "127.0.0.1:4620")
            .header("x-canvas-token", "fixture-token")
            .header("content-type", "application/json")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                12345,
            ))))
            .body(Body::from(body.to_string()))
            .expect("exact HTTP request");
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("HTTP response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("JSON response");
        (
            status,
            serde_json::from_slice(&bytes).expect("response JSON"),
        )
    }

    async fn preflight(&self) {
        let (status, receipt) = self
            .post(
                "/api/models/prepare",
                json!({
                    "id":LEAD,"request_id":REQUEST_ID,"expected_account_key":LEAD_ACCOUNT,
                    "account_key":TARGET,"workers":false
                }),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "metadata preparation must acknowledge before the short settings POST: {receipt}"
        );
        assert_eq!(receipt["id"], LEAD);
        assert_eq!(receipt["request_id"], REQUEST_ID);
        assert_eq!(receipt["status"], "pending");
        assert!(
            receipt.get("catalog").is_none(),
            "catalog data arrives only through SSE"
        );
    }

    async fn terminal(&self) -> Value {
        let resource =
            json!({"kind":"model-catalog-progress","agentId":LEAD,"requestId":REQUEST_ID});
        let mut changes = self.state.sync_changes.subscribe();
        loop {
            changes.borrow_and_update();
            let version = self
                .state
                .resource_notifications
                .published_revision(&resource)
                .expect("canonical receipt resource");
            if version > 0 {
                let mut events = VecDeque::new();
                super::super::sync_stream::append_progress_frames(
                    &self.state,
                    &ResourceChanges {
                        cursor: self.state.sync_changes.snapshot(),
                        resources: vec![resource.clone()],
                        versions: vec![version],
                        entity_reset: false,
                        overflow: false,
                    },
                    &mut events,
                )
                .await;
                if let Some(event) = events.pop_front() {
                    let response =
                        axum::response::Sse::new(futures_util::stream::once(async move {
                            Ok::<_, Infallible>(event)
                        }))
                        .into_response();
                    let bytes = to_bytes(response.into_body(), 1024 * 1024)
                        .await
                        .expect("SSE bytes");
                    let text = std::str::from_utf8(&bytes).expect("SSE UTF8");
                    let frame: Value = serde_json::from_str(
                        text.lines()
                            .find_map(|line| line.strip_prefix("data: "))
                            .expect("SSE data"),
                    )
                    .expect("SSE JSON");
                    assert_eq!(frame["resource"], resource);
                    let receipt = frame["value"].clone();
                    assert_eq!(receipt["id"], LEAD);
                    assert_eq!(receipt["request_id"], REQUEST_ID);
                    if receipt["status"] != "pending" {
                        return receipt;
                    }
                }
            }
            changes
                .changed()
                .await
                .expect("receipt change owner stays live");
        }
    }
}

fn actor(store: &RuntimeStore, id: &str) -> Value {
    let record: String = store
        .test_reader()
        .expect("fixture reader")
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .expect("exact fixture actor");
    serde_json::from_str(&record).expect("actor JSON")
}

fn model_row(store: &RuntimeStore) -> Option<(String, i64)> {
    store.test_reader().expect("fixture reader").query_row(
        "SELECT payload,seq FROM sync_entities WHERE collection='model' AND id=?1 AND deleted=0",
        [format!("{TARGET}:{MODEL}")], |row| Ok((row.get(0)?,row.get(1)?)),
    ).optional().expect("saved model entity")
}

#[tokio::test(start_paused = true)]
async fn same_content_fresh_catalog_preflight_publishes_exact_sse_receipt_before_settings_write() {
    let mut fixture = Fixture::new(RefreshReply::SameContent).await;
    let lead = actor(&fixture.store, LEAD);
    let child = actor(&fixture.store, CHILD);
    let stale_model = model_row(&fixture.store).expect("complete saved model row");
    fixture.preflight().await;
    (&mut fixture.started)
        .await
        .expect("preflight reaches target metadata read");
    let (conflict, _) = fixture
        .post(
            "/api/models/prepare",
            json!({
                "id":LEAD,"request_id":REQUEST_ID,"expected_account_key":LEAD_ACCOUNT,
                "account_key":null,"workers":true
            }),
        )
        .await;
    assert!(
        matches!(conflict, StatusCode::BAD_REQUEST | StatusCode::CONFLICT),
        "the same request identity cannot change an explicit account choice into Automatic"
    );
    tokio::time::advance(Duration::from_secs(
        MODEL_CATALOG_ADMISSION_WAIT_SECONDS + 1,
    ))
    .await;
    assert_eq!(
        actor(&fixture.store, LEAD),
        lead,
        "preflight cannot mutate defaults"
    );
    assert_eq!(actor(&fixture.store, CHILD), child);
    fixture.session.release.add_permits(1);
    let ready = fixture.terminal().await;
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["accountKey"], TARGET);
    assert_eq!(ready["catalog"]["data"][0]["model"], MODEL);
    assert_eq!(
        model_row(&fixture.store),
        Some(stale_model),
        "same-content metadata does not change model entity versions"
    );
    let (status, saved) = fixture.post("/api/conversation",json!({
        "id":LEAD,"expected_account_key":LEAD_ACCOUNT,
        "worker_defaults":{"account_key":TARGET,"model":MODEL,"effort":"medium","fast_mode":false,"daybreak_enabled":false,"delegation_depth":1}
    })).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the unchanged POST validates the ready exact account: {saved}"
    );
    assert_eq!(saved["workerDefaults"]["accountKey"], TARGET);
    assert_eq!(saved["accountKey"], LEAD_ACCOUNT);
    assert_eq!(actor(&fixture.store, CHILD), child);
    assert_eq!(
        fixture.session.reads.load(Ordering::SeqCst),
        2,
        "one prime and one coalesced metadata refresh"
    );
    fixture
        .state
        .model_catalog_handle()
        .shutdown()
        .await
        .expect("metadata tasks finish");
}

#[tokio::test(start_paused = true)]
async fn failed_or_foreign_catalog_metadata_never_authorizes_a_future_worker_choice() {
    for reply in [RefreshReply::Failed, RefreshReply::ForeignScope] {
        let mut fixture = Fixture::new(reply).await;
        let lead = actor(&fixture.store, LEAD);
        let child = actor(&fixture.store, CHILD);
        fixture.preflight().await;
        (&mut fixture.started)
            .await
            .expect("exact metadata request starts");
        fixture.session.release.add_permits(1);
        let receipt = fixture.terminal().await;
        assert_eq!(
            receipt["status"], "unknown",
            "lost or foreign metadata cannot become a reusable success or a new attempt"
        );
        assert!(receipt.get("catalog").is_none_or(Value::is_null));
        assert_eq!(actor(&fixture.store, LEAD), lead);
        assert_eq!(actor(&fixture.store, CHILD), child);
        let (status, replay) = fixture
            .post(
                "/api/models/prepare",
                json!({
                    "id":LEAD,"request_id":REQUEST_ID,"expected_account_key":LEAD_ACCOUNT,
                    "account_key":TARGET,"workers":false
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(replay["status"], "unknown");
        assert!(replay.get("catalog").is_none_or(Value::is_null));
        assert_eq!(
            fixture.session.reads.load(Ordering::SeqCst),
            2,
            "the original unknown receipt must be inspected, not retried"
        );
        fixture
            .state
            .model_catalog_handle()
            .shutdown()
            .await
            .expect("failed metadata tasks finish");
    }
}

#[tokio::test(start_paused = true)]
async fn a_pending_catalog_receipt_cannot_follow_a_changed_actor_or_worker_selection() {
    for changed in ["epoch", "account", "selection"] {
        let mut fixture = Fixture::new(RefreshReply::SameContent).await;
        let child = actor(&fixture.store, CHILD);
        fixture.preflight().await;
        (&mut fixture.started)
            .await
            .expect("original metadata request starts");
        let mut replacement = actor(&fixture.store, LEAD);
        match changed {
            "epoch" => replacement["epoch"] = json!(4),
            "account" => {
                replacement["accountKey"] = json!("old-codex");
                replacement["provider"] = json!("codex");
            }
            "selection" => replacement["workerDefaults"]["model"] = json!("another-model"),
            _ => unreachable!(),
        }
        let stored = replacement.clone();
        fixture
            .store
            .transaction_with(move |tx| AgentRepository::put(tx, &AgentRecord::from_value(stored)?))
            .await
            .expect("fixture authority changes before metadata returns");
        fixture.session.release.add_permits(1);
        let receipt = fixture.terminal().await;
        assert_ne!(
            receipt["status"], "ready",
            "the receipt belongs to its original {changed}"
        );
        assert!(receipt.get("catalog").is_none_or(Value::is_null));
        assert_eq!(actor(&fixture.store, LEAD), replacement);
        assert_eq!(actor(&fixture.store, CHILD), child);
        fixture
            .state
            .model_catalog_handle()
            .shutdown()
            .await
            .expect("metadata tasks finish");
    }
}
