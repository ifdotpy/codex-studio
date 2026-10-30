//! Account-scoped provider model catalogs.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use studio_runtime_providers::ProviderKind;
use studio_runtime_schema::model_catalog::ModelCatalog;

use crate::error::ApiError;
use crate::router::HttpState;
use crate::routes::extractors::QueryParams;
use crate::routes::manifest::{OperationStatus, RouteGroup, get};
use studio_runtime_services::account_policy::AccountsSnapshot;
use studio_runtime_services::model_catalog::{CatalogRead, ModelCatalogService};

// Catalog-read messages shared with branch admission. Spawn admission owns its
// own "no workers were created" wording.
pub(super) const PENDING_MESSAGE: &str =
    "Model catalog is still loading from the provider; retry shortly";
const PENDING_STATUS: StatusCode = StatusCode::SERVICE_UNAVAILABLE;
const WORKER_PENDING_MESSAGE: &str =
    "Model catalog is pending; no worker model catalog is available yet";
pub(super) const UNAVAILABLE_MESSAGE: &str = "Model catalog is temporarily unavailable: the provider did not return its model list; retry shortly";
/// A failed provider read is transient: the catalog retries it on a later read.
const UNAVAILABLE_STATUS: StatusCode = StatusCode::SERVICE_UNAVAILABLE;
const ACCOUNT_UNAVAILABLE_MESSAGE: &str = "Account is unavailable";
pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![get(
        "/api/models",
        OperationStatus::Implemented,
        models,
    )])
}

async fn models(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<(StatusCode, Json<crate::generated::ModelCatalogResponse>), ApiError> {
    let account_key = params
        .nonempty_str("account_key")
        .unwrap_or("default")
        .to_owned();
    let workers = params.typed_value("workers").and_then(Value::as_str) == Some("1");
    let retry = params.typed_value("retry").and_then(Value::as_str) == Some("1");
    let fresh = params.typed_value("fresh").and_then(Value::as_str) == Some("1");
    let account_policy = state
        .account_policy
        .clone()
        .ok_or_else(ApiError::internal)?;
    let catalog = state.model_catalog.clone();
    catalog.set_account_session(state.account_session.clone());
    let driver = state.account_driver.clone();
    let snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/models",
        move || match driver {
            Some(driver) => driver.listing_snapshot().map_err(|_| ApiError::internal()),
            None => Ok(account_policy.accounts()),
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    catalog.retain_public_accounts(&snapshot.accounts);
    let session = state.account_session.clone();
    let result = if workers {
        worker_catalog(&catalog, &snapshot, &account_key, retry, fresh, session).await
    } else if fresh {
        account_catalog_read(&catalog, &snapshot, &account_key, retry, true, session).await
    } else {
        account_catalog(&catalog, &snapshot, &account_key, retry, session).await
    };
    match result {
        Ok(catalog) => {
            // HTTP envelope metadata uses its generated contract; the validated
            // provider models move directly into the response without reparsing.
            let mut response: crate::generated::ModelCatalogResponse =
                serde_json::from_value(Value::Object(catalog.extra))
                    .map_err(|_| ApiError::internal())?;
            response.data = Some(Some(catalog.data));
            response.next_cursor = catalog.next_cursor;
            Ok((StatusCode::OK, Json(response)))
        }
        Err((status, value)) => {
            let response: crate::generated::ModelCatalogResponse =
                serde_json::from_value(value).map_err(|_| ApiError::internal())?;
            Ok((status, Json(response)))
        }
    }
}

async fn account_catalog(
    catalog: &ModelCatalogService,
    snapshot: &AccountsSnapshot,
    account_key: &str,
    retry: bool,
    session: Option<
        std::sync::Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
    >,
) -> Result<ModelCatalog, (StatusCode, Value)> {
    account_catalog_read(catalog, snapshot, account_key, retry, false, session).await
}

async fn account_catalog_read(
    catalog: &ModelCatalogService,
    snapshot: &AccountsSnapshot,
    account_key: &str,
    retry: bool,
    fresh: bool,
    session: Option<
        std::sync::Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
    >,
) -> Result<ModelCatalog, (StatusCode, Value)> {
    if account_key == "default"
        && !snapshot
            .accounts
            .iter()
            .any(|row| row.id.as_str() == Some("default"))
    {
        // Startup can ask for models before any default account exists. The
        // catalog is not ready yet; expose the established pending response.
        return Err(catalog_pending());
    }
    let account = account_context(snapshot, account_key)?;
    if account.provider == ProviderKind::Claude && session.is_none() {
        return Err(catalog_pending());
    }
    let read = async {
        ensure_session(session, &account.key)
            .await
            .map_err(|error| {
                if account.provider == ProviderKind::Claude
                    && matches!(
                    &error,
                    studio_runtime_services::account_session_runtime::AccountSessionError::Requires(
                        _
                    )
                ) {
                    return catalog_pending();
                }
                match error {
                // A refusal is a stable verdict about the account, not a transient fault.
                studio_runtime_services::account_session_runtime::AccountSessionError::Expired => (StatusCode::GONE, json!({"error":error.to_string()})),
                studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                    message,
                ) => (StatusCode::BAD_REQUEST, json!({"error": message})),
                studio_runtime_services::account_session_runtime::AccountSessionError::Requires(_) | studio_runtime_services::account_session_runtime::AccountSessionError::InvalidResponse(_) | studio_runtime_services::account_session_runtime::AccountSessionError::Persistence | studio_runtime_services::account_session_runtime::AccountSessionError::Unknown => catalog_unavailable(),
            }
            })?;
        let result = if fresh {
            if retry {
                let _ = catalog
                    .read(&account.key, &account.home, account.provider, true)
                    .await;
            }
            catalog
                .read_until_fresh(&account.key, &account.home, account.provider)
                .await
        } else {
            catalog
                .read_until_ready(&account.key, &account.home, account.provider, retry)
                .await
        };
        match result {
            CatalogRead::Ready(value) => Ok(value),
            CatalogRead::Pending => Err(catalog_pending()),
            CatalogRead::Unavailable => Err(catalog_unavailable()),
        }
    };
    if fresh {
        // Finish the catalog preflight before the short settings POST starts.
        tokio::time::timeout(
            std::time::Duration::from_secs(
                studio_runtime_schema::shared_values::MODEL_CATALOG_OPERATION_TIMEOUT_SECONDS,
            ),
            read,
        )
        .await
        .unwrap_or_else(|_| Err(catalog_pending()))
    } else {
        read.await
    }
}

fn catalog_unavailable() -> (StatusCode, Value) {
    (UNAVAILABLE_STATUS, json!({"error": UNAVAILABLE_MESSAGE}))
}

fn catalog_pending() -> (StatusCode, Value) {
    (
        PENDING_STATUS,
        json!({"error": PENDING_MESSAGE, "catalogPending": true}),
    )
}

async fn ensure_session(
    session: Option<
        std::sync::Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
    >,
    account: &str,
) -> Result<(), studio_runtime_services::account_session_runtime::AccountSessionError> {
    let session = session.ok_or(
        studio_runtime_services::account_session_runtime::AccountSessionError::Requires(
            "provider-session/account-session",
        ),
    )?;
    session.ensure_session(account).await?;
    Ok(())
}

struct AccountContext {
    key: String,
    home: String,
    provider: ProviderKind,
}

fn account_context(
    snapshot: &AccountsSnapshot,
    account_key: &str,
) -> Result<AccountContext, (StatusCode, Value)> {
    let Some(account) = snapshot
        .accounts
        .iter()
        .find(|row| row.id.as_str() == Some(account_key))
    else {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": "Unknown Codex account"}),
        ));
    };
    if matches!(account.status.as_str(), Some("changed" | "error")) {
        let message = account
            .error
            .as_str()
            .unwrap_or(ACCOUNT_UNAVAILABLE_MESSAGE);
        return Err((StatusCode::BAD_REQUEST, json!({"error": message})));
    }
    Ok(AccountContext {
        key: account_key.to_owned(),
        home: account.home.as_str().unwrap_or_default().to_owned(),
        provider: account.provider_kind().ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                json!({"error": ACCOUNT_UNAVAILABLE_MESSAGE}),
            )
        })?,
    })
}

async fn worker_catalog(
    catalog: &ModelCatalogService,
    snapshot: &AccountsSnapshot,
    parent_account: &str,
    retry: bool,
    fresh: bool,
    session: Option<
        std::sync::Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
    >,
) -> Result<ModelCatalog, (StatusCode, Value)> {
    let default_account = snapshot.default_account_key.as_str().unwrap_or("default");
    if parent_account == "default" && snapshot.accounts.is_empty() {
        return Err(catalog_pending());
    }
    let mut order = Vec::new();
    for key in [parent_account, default_account]
        .into_iter()
        .chain(snapshot.accounts.iter().filter_map(|row| row.id.as_str()))
    {
        if !order.iter().any(|existing| existing == key) {
            order.push(key.to_owned());
        }
    }

    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut unavailable = Vec::new();
    let mut pending = false;
    let mut selection_reads = Vec::new();
    for key in order {
        let Some(source) = snapshot
            .accounts
            .iter()
            .find(|row| row.id.as_str() == Some(key.as_str()))
        else {
            continue;
        };
        if source.disconnected.as_bool() == Some(true)
            || source.deleted.as_bool() == Some(true)
            || source.duplicate_of.is_present_non_null()
            || (key != parent_account
                && source.status.as_str() != Some("ready")
                && source.can_attempt_native_proof.as_bool() != Some(true))
        {
            continue;
        }
        let account = match account_context(snapshot, &key) {
            Ok(account) => account,
            Err((_, error)) => {
                unavailable.push(json!({"accountKey": key, "error": error["error"]}));
                continue;
            }
        };
        if fresh {
            let session = session.clone();
            selection_reads.push(async move {
                let result =
                    account_catalog_read(catalog, snapshot, &account.key, retry, true, session)
                        .await;
                (account, result)
            });
            continue;
        }
        if let Err(error) = ensure_session(session.clone(), &account.key).await {
            unavailable.push(json!({"accountKey":key,"error":error.to_string()}));
            continue;
        }
        match catalog
            .read(&account.key, &account.home, account.provider, retry)
            .await
        {
            CatalogRead::Ready(value) => {
                append_worker_models(&mut models, &mut seen, &account, value);
            }
            CatalogRead::Pending => {
                pending = true;
                unavailable.push(json!({
                    "accountKey": key,
                    "error": PENDING_MESSAGE,
                    "catalogPending": true,
                }));
            }
            CatalogRead::Unavailable => unavailable.push(json!({
                "accountKey": key,
                "error": UNAVAILABLE_MESSAGE,
            })),
        }
    }
    // Candidate reads share the user action's provider budget and keep the
    // existing account preference order. Display reads keep their old path.
    let completed = futures_util::future::join_all(selection_reads).await;
    let identities = completed
        .iter()
        .map(|(account, _)| {
            (
                account.key.as_str(),
                account.home.as_str(),
                account.provider,
            )
        })
        .collect::<Vec<_>>();
    let current = catalog.read_for_admission_batch(&identities);
    for ((account, result), current) in completed.into_iter().zip(current) {
        // A candidate can retire or fail while another account's read waits.
        // Publish only the final coherent cache snapshot, never the captured value.
        let result = match (result, current) {
            (
                Ok(_),
                studio_runtime_services::model_catalog::AdmissionCatalogRead::Fresh {
                    value, ..
                },
            ) => Ok(value),
            (Ok(_), studio_runtime_services::model_catalog::AdmissionCatalogRead::Pending) => {
                Err(catalog_pending().1)
            }
            (Ok(_), studio_runtime_services::model_catalog::AdmissionCatalogRead::Unavailable) => {
                Err(catalog_unavailable().1)
            }
            (Err((_, error)), _) => Err(error),
        };
        match result {
            Ok(value) => append_worker_models(&mut models, &mut seen, &account, value),
            Err(error) => {
                let account_pending = error.get("catalogPending") == Some(&Value::Bool(true));
                pending |= account_pending;
                let mut unavailable_account =
                    json!({"accountKey":account.key, "error":error["error"]});
                if account_pending {
                    unavailable_account["catalogPending"] = Value::Bool(true);
                }
                unavailable.push(unavailable_account);
            }
        }
    }
    if models.is_empty() {
        if pending {
            return Err((
                PENDING_STATUS,
                json!({"error": WORKER_PENDING_MESSAGE, "catalogPending": true}),
            ));
        }
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": "No worker model catalog is available"}),
        ));
    }
    let mut extra = serde_json::Map::new();
    extra.insert("unavailableAccounts".into(), json!(unavailable));
    if pending {
        extra.insert("catalogPending".into(), Value::Bool(true));
    }
    Ok(ModelCatalog {
        data: models,
        next_cursor: None,
        extra,
    })
}

fn append_worker_models(
    models: &mut Vec<studio_runtime_schema::model_catalog::ProviderModel>,
    seen: &mut std::collections::HashSet<String>,
    account: &AccountContext,
    value: ModelCatalog,
) {
    for mut row in value.data {
        if row.hidden == Some(true) || !seen.insert(row.model.clone()) {
            continue;
        }
        row.account_key = Some(account.key.clone());
        row.provider = Some(account.provider);
        models.push(row);
    }
}

#[cfg(test)]
mod tests {
    use super::{PENDING_MESSAGE, account_catalog, account_context};
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use studio_runtime_providers::ProviderKind;
    use studio_runtime_services::account_policy::AccountsSnapshot;
    use studio_runtime_services::model_catalog::ModelCatalogService;

    struct EnsuredSession(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl studio_runtime_services::account_session_runtime::AccountSessionPort for EnsuredSession {
        fn ensure_session<'a>(
            &'a self,
            account: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            studio_runtime_services::account_session_runtime::ConnectionScope,
                            studio_runtime_services::account_session_runtime::AccountSessionError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let account = account.to_owned();
            Box::pin(async move {
                Ok(
                    studio_runtime_services::account_session_runtime::ConnectionScope {
                        account,
                        connection: "fixture-connection".into(),
                        generation: 1,
                    },
                )
            })
        }
    }

    struct CatalogSession {
        pages: std::sync::Mutex<Vec<Value>>,
        requests: std::sync::Arc<AtomicUsize>,
    }

    impl studio_runtime_services::account_session_runtime::AccountSessionPort for CatalogSession {
        fn ensure_session<'a>(
            &'a self,
            account: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            studio_runtime_services::account_session_runtime::ConnectionScope,
                            studio_runtime_services::account_session_runtime::AccountSessionError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let scope = studio_runtime_services::account_session_runtime::ConnectionScope {
                account: account.to_owned(),
                connection: "fixture-connection".into(),
                generation: 1,
            };
            Box::pin(async move { Ok(scope) })
        }

        fn current_scope(
            &self,
            account: &str,
        ) -> Option<studio_runtime_services::account_session_runtime::ConnectionScope> {
            Some(
                studio_runtime_services::account_session_runtime::ConnectionScope {
                    account: account.to_owned(),
                    connection: "fixture-connection".into(),
                    generation: 1,
                },
            )
        }

        fn provider_request<'a>(
            &'a self,
            request: studio_runtime_services::account_session_runtime::ProviderRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            studio_runtime_services::account_session_runtime::SessionReceipt,
                            studio_runtime_services::account_session_runtime::AccountSessionError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            use studio_runtime_services::account_session_runtime::SessionOutcome as ReceiptOutcome;
            use studio_runtime_services::account_session_runtime::{
                ProviderResponse, SessionReceipt,
            };

            self.requests.fetch_add(1, Ordering::SeqCst);
            let page = self.pages.lock().expect("pages").pop();
            let scope = self.current_scope(&request.account).expect("scope");
            let operation = request.operation;
            Box::pin(async move {
                let page = page.ok_or_else(|| {
                    studio_runtime_services::account_session_runtime::AccountSessionError::InvalidResponse(
                        "/".into(),
                    )
                })?;
                Ok(SessionReceipt {
                    scope,
                    operation,
                    replayed: false,
                    outcome: ReceiptOutcome::Written {
                        ticket: studio_runtime_providers::RequestTicket {
                            id: studio_runtime_providers::RequestId::String("catalog-test".into()),
                            generation: 1,
                            method: studio_runtime_providers::Method::ModelList,
                        },
                        response: ProviderResponse::Reply(page),
                    },
                })
            })
        }
    }

    #[tokio::test]
    async fn registration_publishes_model_entities_to_subscribers() {
        use axum::{body::Body, extract::ConnectInfo, http::Request};
        use futures_util::StreamExt;
        use studio_runtime_services::account_session_runtime::{
            AccountSessionFuture, AccountSessionPort, ConnectionScope, ProviderRequest,
            SessionReceipt,
        };
        use tower::ServiceExt;
        struct GatedCatalog {
            inner: CatalogSession,
            gate: tokio::sync::Notify,
        }
        impl AccountSessionPort for GatedCatalog {
            fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
                self.inner.current_scope(account)
            }
            fn provider_request<'a>(
                &'a self,
                request: ProviderRequest,
            ) -> AccountSessionFuture<'a, SessionReceipt> {
                Box::pin(async move {
                    self.gate.notified().await;
                    self.inner.provider_request(request).await
                })
            }
        }
        let root = tempfile::tempdir().expect("isolated state");
        let store = std::sync::Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(root.path()))
                .await
                .expect("store"),
        );
        let registry = root.path().join("registry.json");
        let registry_data = json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,"defaultAccountKey":"default","accounts":{},"logins":{}}).to_string();
        tokio::fs::write(&registry, &registry_data)
            .await
            .expect("registry");
        let driver = std::sync::Arc::new(
            studio_runtime_services::account_driver::AccountDriver::open(registry).expect("driver"),
        );
        let policy = studio_runtime_services::account_policy::AccountPolicyState::from_startup(
            Some(registry_data.as_bytes()),
            [],
        )
        .expect("policy");
        let home = root.path().join("profile");
        tokio::fs::create_dir(&home).await.expect("profile");
        tokio::fs::write(home.join("auth.json"), r#"{"OPENAI_API_KEY":"fixture"}"#)
            .await
            .expect("auth");
        let port = std::sync::Arc::new(GatedCatalog {
            inner: CatalogSession {
                pages: std::sync::Mutex::new(vec![json!({"data":[{"model":"fixture-model"}]})]),
                requests: std::sync::Arc::new(AtomicUsize::new(0)),
            },
            gate: tokio::sync::Notify::new(),
        });
        let mut state = crate::router::HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store),
            Some(std::sync::Arc::new(policy)),
        )
        .with_account_driver(driver);
        state.account_session = Some(port.clone());
        state.model_catalog.set_account_session(Some(port.clone()));
        let app = crate::router::router(state).expect("router");
        let request = Request::builder()
            .method("POST")
            .uri("/api/accounts/register")
            .header("host", "127.0.0.1:4321")
            .header("content-type", "application/json")
            .header("x-canvas-token", "token")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .extension(ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                12345,
            ))))
            .body(Body::from(json!({"home":home}).to_string()))
            .expect("registration");
        assert_eq!(
            app.clone()
                .oneshot(request)
                .await
                .expect("registration response")
                .status(),
            StatusCode::OK
        );
        let request = Request::builder().uri(format!("/api/sync/stream?protocol={}&resources=%5B%7B%22kind%22%3A%22state%22%7D%5D&tokenRates=false", studio_runtime_schema::sync_protocol::SYNC_PROTOCOL_VERSION)).header("host", "127.0.0.1:4321").header("x-canvas-token", "token").extension(ConnectInfo(std::net::SocketAddr::from(([127,0,0,1],12345)))).body(Body::empty()).expect("stream request");
        let response = app.oneshot(request).await.expect("stream response");
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        stream.next().await.expect("session").expect("bytes");
        stream.next().await.expect("initial").expect("bytes");
        port.gate.notify_one();
        let found = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while let Some(Ok(frame)) = stream.next().await {
                if String::from_utf8_lossy(&frame).contains("fixture-model") {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(
            found,
            "registered account model must arrive over SSE without a catalog GET"
        );
    }

    fn snapshot(account: Value) -> AccountsSnapshot {
        AccountsSnapshot {
            accounts: vec![serde_json::from_value(account).expect("public account fixture")],
            archived_accounts: Vec::new(),
            default_account_key: json!("default"),
            default_account_groups: Vec::new(),
            logins: Vec::new(),
            known_account_keys: HashSet::new(),
        }
    }

    struct DelayedRefreshSession {
        inner: CatalogSession,
        gate: tokio::sync::Semaphore,
        requested: tokio::sync::Notify,
    }

    impl studio_runtime_services::account_session_runtime::AccountSessionPort
        for DelayedRefreshSession
    {
        fn current_scope(
            &self,
            account: &str,
        ) -> Option<studio_runtime_services::account_session_runtime::ConnectionScope> {
            self.inner.current_scope(account)
        }

        fn ensure_session<'a>(
            &'a self,
            account: &'a str,
        ) -> studio_runtime_services::account_session_runtime::AccountSessionFuture<
            'a,
            studio_runtime_services::account_session_runtime::ConnectionScope,
        > {
            self.inner.ensure_session(account)
        }

        fn provider_request<'a>(
            &'a self,
            request: studio_runtime_services::account_session_runtime::ProviderRequest,
        ) -> studio_runtime_services::account_session_runtime::AccountSessionFuture<
            'a,
            studio_runtime_services::account_session_runtime::SessionReceipt,
        > {
            let reply = self.inner.provider_request(request);
            let refresh = self.inner.requests.load(Ordering::SeqCst) > 1;
            Box::pin(async move {
                if refresh {
                    self.requested.notify_one();
                    self.gate.acquire().await.expect("refresh gate").forget();
                }
                reply.await
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fresh_account_selection_waits_for_the_shared_refresh_while_display_keeps_stale_rows() {
        use axum::{
            body::{Body, to_bytes},
            http::Request,
        };
        use std::{sync::Arc, time::Duration};
        use studio_runtime_services::model_catalog::{AdmissionCatalogRead, CatalogRead};
        use tower::ServiceExt;

        for provider in [ProviderKind::Codex, ProviderKind::Claude] {
            for workers in [false, true] {
                let startup = serde_json::to_vec(&json!({
                "version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION, "defaultAccountKey":"target", "logins":{},
                "accounts":{"target":{"id":"target","home":"fixture","provider":provider,"label":"Target","source":"fixture","status":"ready"}}
            })).expect("startup account");
                let policy = Arc::new(
                    studio_runtime_services::account_policy::AccountPolicyState::from_startup(
                        Some(&startup),
                        [],
                    )
                    .expect("account policy"),
                );
                let session = Arc::new(DelayedRefreshSession {
                    inner: CatalogSession {
                        pages: std::sync::Mutex::new(vec![
                            json!({"data":[{"model":"fresh"}]}),
                            json!({"data":[{"model":"stale"}]}),
                        ]),
                        requests: Arc::new(AtomicUsize::new(0)),
                    },
                    gate: tokio::sync::Semaphore::new(0),
                    requested: tokio::sync::Notify::new(),
                });
                let mut state = crate::router::HttpState::new(
                    "token".into(),
                    "fixture".into(),
                    "/fixture".into(),
                    4000,
                    None,
                    None,
                    Some(policy),
                );
                state.account_session = Some(session.clone());
                state
                    .model_catalog
                    .set_account_session(Some(session.clone()));
                let catalog = state.model_catalog.clone();
                assert!(matches!(
                    catalog
                        .read_until_ready("target", "fixture", provider, false)
                        .await,
                    CatalogRead::Ready(_)
                ));
                tokio::time::advance(Duration::from_secs(
                    studio_runtime_schema::shared_values::MODEL_CATALOG_CACHE_TTL_SECONDS + 1,
                ))
                .await;
                let app = crate::router::router(state).expect("HTTP router");
                let request = |uri: &str| {
                    Request::builder()
                        .uri(uri)
                        .header("host", "127.0.0.1:4000")
                        .header("x-canvas-token", "token")
                        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                            [127, 0, 0, 1],
                            12345,
                        ))))
                        .body(Body::empty())
                        .expect("catalog request")
                };
                let display = app
                    .clone()
                    .oneshot(request("/api/models?account_key=target"))
                    .await
                    .expect("display response");
                assert_eq!(display.status(), StatusCode::OK);
                let display: Value = serde_json::from_slice(
                    &to_bytes(display.into_body(), 65536)
                        .await
                        .expect("display body"),
                )
                .expect("display JSON");
                assert_eq!(display["data"][0]["model"], "stale");
                session.requested.notified().await;
                let selection_uri = format!(
                    "/api/models?account_key=target&fresh=1{}",
                    if workers { "&workers=1" } else { "" }
                );
                let fresh = tokio::spawn(app.oneshot(request(&selection_uri)));
                tokio::task::yield_now().await;
                tokio::time::advance(Duration::from_secs(
                    studio_runtime_schema::shared_values::MODEL_CATALOG_ADMISSION_WAIT_SECONDS + 1,
                ))
                .await;
                tokio::task::yield_now().await;
                assert!(
                    !fresh.is_finished(),
                    "selection must await the fresh catalog instead of returning stale rows or refusing a slow refresh"
                );
                session.gate.add_permits(1);
                let response = fresh
                    .await
                    .expect("selection task")
                    .expect("selection response");
                assert_eq!(response.status(), StatusCode::OK);
                let body: Value = serde_json::from_slice(
                    &to_bytes(response.into_body(), 65536)
                        .await
                        .expect("selection body"),
                )
                .expect("selection JSON");
                assert_eq!(body["data"][0]["model"], "fresh");
                assert!(matches!(
                    catalog.read_for_admission("target", "fixture", provider),
                    AdmissionCatalogRead::Fresh { .. }
                ));
                assert_eq!(
                    session.inner.requests.load(Ordering::SeqCst),
                    2,
                    "display and selection share one refresh"
                );
                catalog.shutdown().await.expect("catalog shutdown");
            }
        }
    }

    #[tokio::test]
    async fn startup_model_catalog_without_a_configured_default_is_pending() {
        let mut snapshot = snapshot(json!({}));
        snapshot.accounts.clear();
        for fresh in [false, true] {
            let error = super::account_catalog_read(
                &ModelCatalogService::new(),
                &snapshot,
                "default",
                false,
                fresh,
                None,
            )
            .await
            .expect_err("an unconfigured account has no catalog yet");
            assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(error.1, super::catalog_pending().1);
        }
    }

    #[tokio::test]
    async fn changed_and_error_accounts_are_refused_before_catalog_read() {
        for status in ["changed", "error"] {
            let snapshot = snapshot(
                json!({"id":"default", "home":"/missing", "provider":studio_runtime_schema::ProviderKind::Codex, "status":status}),
            );
            let error = account_catalog(
                &ModelCatalogService::new(),
                &snapshot,
                "default",
                true,
                None,
            )
            .await
            .expect_err("changed account must be refused");
            assert_eq!(error.0, StatusCode::BAD_REQUEST);
            assert_eq!(error.1, json!({"error":"Account is unavailable"}));
        }
    }

    #[tokio::test]
    async fn changed_account_is_refused_with_its_own_message_for_every_provider() {
        let message = studio_runtime_services::account_driver::CHANGED_ERROR;
        for provider in ["claude", "codex"] {
            let snapshot = snapshot(json!({
                "id":"profile", "provider":provider, "status":"changed", "error":message
            }));
            let error = account_catalog(
                &ModelCatalogService::new(),
                &snapshot,
                "profile",
                true,
                None,
            )
            .await
            .expect_err("changed account must be refused");
            assert_eq!(error.0, StatusCode::BAD_REQUEST);
            assert_eq!(error.1, json!({"error":message}));
        }
    }

    #[tokio::test]
    async fn claude_catalog_without_a_session_is_pending_after_account_validation() {
        let snapshot =
            snapshot(json!({"id":"claude-account", "provider":"claude", "status":"ready"}));
        let error = account_catalog(
            &ModelCatalogService::new(),
            &snapshot,
            "claude-account",
            false,
            None,
        )
        .await
        .expect_err("Claude requires its provider service");
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            error.1,
            json!({"error":PENDING_MESSAGE,"catalogPending":true})
        );
    }

    #[tokio::test]
    async fn catalog_ensures_account_session_before_model_catalog_read() {
        let snapshot = snapshot(
            json!({"id":"default", "home":"/fixture", "provider":"codex", "status":"ready"}),
        );
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let session: std::sync::Arc<
            dyn studio_runtime_services::account_session_runtime::AccountSessionPort,
        > = std::sync::Arc::new(EnsuredSession(calls.clone()));
        let error = account_catalog(
            &ModelCatalogService::new(),
            &snapshot,
            "default",
            false,
            Some(session),
        )
        .await
        .expect_err("model catalog provider service is absent in this fixture");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.1, json!({"error":super::UNAVAILABLE_MESSAGE}));
    }

    #[tokio::test]
    async fn cold_account_catalog_waits_for_one_shared_provider_read() {
        let account = json!({
            "id":"claude-account",
            "home":"/fixture",
            "provider":"claude",
            "status":"ready"
        });
        let snapshot = snapshot(account);
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let session: std::sync::Arc<
            dyn studio_runtime_services::account_session_runtime::AccountSessionPort,
        > = std::sync::Arc::new(CatalogSession {
            pages: std::sync::Mutex::new(vec![json!({
                "data":[{"model":"fixture-model","defaultReasoningEffort":null,"vendor":{"future":true}}],
                "metadata":{"futurePage":true},
                "nextCursor":null
            })]),
            requests: calls.clone(),
        });
        let catalog = ModelCatalogService::new();
        catalog.set_account_session(Some(session.clone()));

        let (first, second) = tokio::join!(
            account_catalog(
                &catalog,
                &snapshot,
                "claude-account",
                false,
                Some(session.clone())
            ),
            account_catalog(&catalog, &snapshot, "claude-account", false, Some(session)),
        );

        let expected = json!({
            "data":[{"model":"fixture-model","defaultReasoningEffort":null,"vendor":{"future":true}}],
                "metadata":{"futurePage":true},
            "nextCursor":null
        });
        assert_eq!(
            serde_json::to_value(first.expect("cold catalog waits for 200 response"))
                .expect("serialized catalog"),
            expected
        );
        assert_eq!(
            serde_json::to_value(second.expect("concurrent catalog shares result"))
                .expect("serialized catalog"),
            expected
        );
        let document: Value =
            serde_json::from_str(include_str!("../../openapi.json")).expect("OpenAPI document");
        let schema = json!({
            "components":document["components"],
            "$ref":"#/components/schemas/ModelCatalogResponse"
        });
        assert!(
            jsonschema::validator_for(&schema)
                .expect("HTTP schema")
                .is_valid(&expected)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        catalog.shutdown().await.expect("catalog task shutdown");
    }

    #[tokio::test]
    async fn invalid_known_provider_fields_are_unavailable_to_http_and_admission() {
        let snapshot = snapshot(json!({
            "id":"account", "home":"/fixture", "provider":"codex", "status":"ready"
        }));
        let session: std::sync::Arc<
            dyn studio_runtime_services::account_session_runtime::AccountSessionPort,
        > = std::sync::Arc::new(CatalogSession {
            pages: std::sync::Mutex::new(vec![json!({
                "data":[{"model":"valid", "supportedReasoningEfforts":[{"reasoningEffort":1}]}]
            })]),
            requests: Default::default(),
        });
        let catalog = ModelCatalogService::new();
        catalog.set_account_session(Some(session.clone()));
        let error = account_catalog(&catalog, &snapshot, "account", false, Some(session))
            .await
            .expect_err("invalid capabilities must fail the HTTP read");
        assert_eq!(error, super::catalog_unavailable());
        assert_eq!(
            catalog.read_for_admission("account", "/fixture", ProviderKind::Codex),
            studio_runtime_services::model_catalog::AdmissionCatalogRead::Unavailable
        );
        catalog.shutdown().await.expect("catalog task shutdown");
    }

    #[tokio::test]
    async fn failed_cold_account_catalog_remains_unavailable() {
        let snapshot = snapshot(json!({
            "id":"claude-account",
            "home":"/fixture",
            "provider":"claude",
            "status":"ready"
        }));
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let session: std::sync::Arc<
            dyn studio_runtime_services::account_session_runtime::AccountSessionPort,
        > = std::sync::Arc::new(CatalogSession {
            pages: std::sync::Mutex::new(Vec::new()),
            requests: calls.clone(),
        });
        let catalog = ModelCatalogService::new();
        catalog.set_account_session(Some(session.clone()));

        let error = account_catalog(&catalog, &snapshot, "claude-account", false, Some(session))
            .await
            .expect_err("failed provider read stays unavailable");

        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.1, json!({"error":super::UNAVAILABLE_MESSAGE}));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        catalog.shutdown().await.expect("catalog task shutdown");
    }

    #[test]
    fn account_home_and_provider_are_projected_together() {
        let snapshot = snapshot(
            json!({"id":"profile", "home":"/profile", "provider":"codex", "status":"ready"}),
        );
        let context = account_context(&snapshot, "profile").expect("profile exists");
        assert_eq!(context.key, "profile");
        assert_eq!(context.home, "/profile");
        assert_eq!(context.provider, ProviderKind::Codex);
    }

    #[test]
    fn route_manifest_declares_model_catalog_as_implemented() {
        let operation = crate::routes::manifest::operation_manifest()
            .expect("route manifest validates")
            .into_iter()
            .find(|operation| operation.key() == ("GET".to_owned(), "/api/models".to_owned()))
            .expect("models operation is registered");
        assert_eq!(
            serde_json::to_value(operation).expect("operation serializes"),
            json!({
                "method":"GET",
                "path":"/api/models",
                "status":"implemented"
            })
        );
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "models",
        routes: Some(routes),
        dependencies: &[],
    }
}
