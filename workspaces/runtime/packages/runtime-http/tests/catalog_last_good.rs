//! Worker creation survives a slow or failed model catalog read when a recent
//! last good catalog of the account can validate the requested model.
#![allow(clippy::disallowed_methods)]

use serde_json::json;
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use studio_runtime_providers::{Method, ProviderKind, RequestId, RequestTicket};
use studio_runtime_schema::shared_values::{
    MODEL_CATALOG_CACHE_TTL_SECONDS, MODEL_CATALOG_LAST_GOOD_MAX_AGE_SECONDS,
};
use studio_runtime_services::{
    account_driver::AccountDriver,
    account_session_runtime::{
        AccountSessionError, AccountSessionFuture, AccountSessionPort, ConnectionScope,
        ProviderRequest, ProviderResponse, SessionOutcome, SessionReceipt,
    },
    model_catalog::{CatalogRead, ModelCatalogService},
    provider_callbacks::{
        ManagedCreationEnvironment, ManagedCreationEnvironmentError, WorkerAccountRequest,
    },
};

const ACCOUNT: &str = "worker-codex";
const MODEL: &str = "gpt-6.1-sol";

struct Session {
    failing: AtomicBool,
}

impl AccountSessionPort for Session {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        Some(ConnectionScope {
            account: account.into(),
            connection: "fixture".into(),
            generation: 7,
        })
    }
    fn ensure_session<'a>(&'a self, account: &'a str) -> AccountSessionFuture<'a, ConnectionScope> {
        Box::pin(async move { Ok(self.current_scope(account).expect("scope")) })
    }
    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            if self.failing.load(Ordering::SeqCst) {
                return Err(AccountSessionError::Unknown);
            }
            let scope = self.current_scope(&request.account).expect("scope");
            Ok(SessionReceipt {
                scope,
                operation: request.operation,
                outcome: SessionOutcome::Written {
                    ticket: RequestTicket {
                        generation: 7,
                        id: RequestId::String("catalog".into()),
                        method: Method::ModelList,
                    },
                    response: ProviderResponse::Reply(json!({"data":[{
                        "model":MODEL,"supportedReasoningEfforts":[{"reasoningEffort":"medium"}],
                        "defaultReasoningEffort":"medium","serviceTiers":[]
                    }],"nextCursor":null})),
                },
                replayed: false,
            })
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    session: Arc<Session>,
    catalog: ModelCatalogService,
    environment: Arc<dyn ManagedCreationEnvironment>,
    home: String,
}

impl Fixture {
    /// `failing` makes every provider read fail from the start.
    async fn new(failing: bool) -> Self {
        let root = tempfile::tempdir().expect("fixture root");
        let cwd =
            studio_runtime_store::path_identity::resolve(root.path()).expect("canonical fixture");
        let home = cwd.join("home");
        std::fs::create_dir_all(&home).expect("profile");
        std::fs::write(home.join("auth.json"), json!({"OPENAI_API_KEY":"k"}).to_string())
            .expect("auth");
        let registry = json!({
            "version": studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,
            "defaultAccountKey": ACCOUNT,
            "accounts": {ACCOUNT: {"id":ACCOUNT,"home":home,"provider":"codex","label":"Worker",
                "source":"Fixture","status":"ready"}},
            "logins": {}, "deleteReceipts": {}
        });
        let path = cwd.join("accounts.json");
        std::fs::write(&path, serde_json::to_vec(&registry).expect("registry")).expect("write");
        let driver = Arc::new(AccountDriver::open(&path).expect("account driver"));
        let session = Arc::new(Session {
            failing: AtomicBool::new(failing),
        });
        let catalog = ModelCatalogService::new();
        catalog.set_account_session(Some(session.clone()));
        catalog.retain_accounts(&driver.snapshot().expect("accounts").accounts);
        let environment =
            studio_runtime_http::routes::managed_creation_environment(driver, catalog.clone());
        Self {
            _root: root,
            session,
            catalog,
            environment,
            home: home.to_string_lossy().into_owned(),
        }
    }

    async fn read_good_catalog(&self) {
        assert!(matches!(
            self.catalog
                .read_until_ready(ACCOUNT, &self.home, ProviderKind::Codex, false)
                .await,
            CatalogRead::Ready(_)
        ));
    }

    /// The fresh cache entry expires and the next provider read fails.
    async fn expire_fresh_catalog_and_fail_reads(&self, age: Duration) {
        self.session.failing.store(true, Ordering::SeqCst);
        tokio::time::advance(age).await;
    }

    async fn spawn_decision(
        &self,
        model: &str,
    ) -> Result<(), ManagedCreationEnvironmentError> {
        self.environment.wait_for_catalog(ACCOUNT).await?;
        let active = HashMap::new();
        let (key, identity) = self.environment.select_worker_account(WorkerAccountRequest {
            parent_account_key: ACCOUNT,
            requested_account_key: Some(ACCOUNT),
            default_account_key: None,
            model,
            excluded_account_keys: &[],
            project_account_keys: None,
            project_account_groups: None,
            active_agents: &active,
        })?;
        // A review resolves its reviewer model through the same catalog owner.
        self.environment
            .catalog()
            .resolve(&key, &identity, model, None, false, false)
            .map(|_| ())
            .map_err(|_| ManagedCreationEnvironmentError::Invalid("model"))
    }
}

const PAST_TTL: Duration = Duration::from_secs(MODEL_CATALOG_CACHE_TTL_SECONDS + 1);

#[tokio::test(start_paused = true)]
async fn spawn_proceeds_on_last_good_catalog_when_fresh_read_fails() {
    let fixture = Fixture::new(false).await;
    fixture.read_good_catalog().await;
    fixture.expire_fresh_catalog_and_fail_reads(PAST_TTL).await;
    assert_eq!(fixture.spawn_decision(MODEL).await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn spawn_refuses_a_model_missing_from_the_last_good_catalog() {
    let fixture = Fixture::new(false).await;
    fixture.read_good_catalog().await;
    fixture.expire_fresh_catalog_and_fail_reads(PAST_TTL).await;
    assert_eq!(
        fixture.spawn_decision("gpt-unknown").await,
        Err(ManagedCreationEnvironmentError::Invalid(
            "This model is not available for the selected worker accounts"
        ))
    );
}

#[tokio::test(start_paused = true)]
async fn spawn_refuses_when_no_catalog_was_ever_read() {
    let fixture = Fixture::new(true).await;
    assert_eq!(
        fixture.spawn_decision(MODEL).await,
        Err(ManagedCreationEnvironmentError::Unavailable(
            "provider-session/model-list"
        ))
    );
}

#[tokio::test(start_paused = true)]
async fn spawn_refuses_when_the_last_good_catalog_is_too_old() {
    let fixture = Fixture::new(false).await;
    fixture.read_good_catalog().await;
    fixture
        .expire_fresh_catalog_and_fail_reads(Duration::from_secs(
            MODEL_CATALOG_LAST_GOOD_MAX_AGE_SECONDS + 1,
        ))
        .await;
    assert_eq!(
        fixture.spawn_decision(MODEL).await,
        Err(ManagedCreationEnvironmentError::Unavailable(
            "provider-session/model-list"
        ))
    );
}
