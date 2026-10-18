//! Bounded, process-local model catalog reads for display.

use crate::provider_exchange;
use crate::provider_session_task::{
    RequestSpec, SessionHandle, SessionOutcome, SessionTaskError, WriteOutcome,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(test)]
use studio_runtime_process::provider_seam::ProviderTaskFactory;
#[cfg(test)]
use studio_runtime_process::{Generation, ProviderHandle, ProviderLaunchSpec};
use studio_runtime_process::{
    OperationId,
    operational_log::OperationalLog,
    provider_seam::{InitializationStatus, Shutdown},
};
use studio_runtime_providers::{Method, RequestId, RequestOutcome, ResponseBody};
use studio_runtime_schema::shared_values::{
    MODEL_CATALOG_ADMISSION_WAIT_SECONDS, MODEL_CATALOG_CACHE_TTL_SECONDS,
    MODEL_CATALOG_ITEMS_PER_PAGE, MODEL_CATALOG_MAX_FRAME_BYTES, MODEL_CATALOG_MAX_PAGES,
    MODEL_CATALOG_MAX_RESPONSE_BYTES, MODEL_CATALOG_OPERATION_TIMEOUT_SECONDS,
};
use tokio::time::Instant;

const CATALOG_TTL: Duration = Duration::from_secs(MODEL_CATALOG_CACHE_TTL_SECONDS);
/// How long one failed provider refresh answers reads as unavailable before the
/// next read starts another refresh. Owner: this cache. Unit: seconds. Kept short
/// so a transient failure (a cold provider start under load, an idle-closed
/// session) does not hide the catalog for the full success TTL.
const CATALOG_FAILURE_RETRY: Duration = Duration::from_secs(5);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(MODEL_CATALOG_OPERATION_TIMEOUT_SECONDS);
const CATALOG_ADMISSION_WAIT: Duration = Duration::from_secs(MODEL_CATALOG_ADMISSION_WAIT_SECONDS);
const MAX_CATALOG_PAGE_BYTES: usize = MODEL_CATALOG_MAX_FRAME_BYTES;
const MAX_CATALOG_RESPONSE_BYTES: usize = MODEL_CATALOG_MAX_RESPONSE_BYTES;
const MAX_CATALOG_PAGES: usize = MODEL_CATALOG_MAX_PAGES;
pub type ChangeCallback = Arc<dyn Fn() + Send + Sync + 'static>;

/// Dependency name reported when no fresh model catalog can be read for an
/// admission decision. The condition is transient: a later read can succeed.
pub const MODEL_CATALOG_DEPENDENCY: &str = "provider-session/model-list";

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelListError {
    #[error("Unknown account")]
    UnknownAccount,
    #[error("Account provider changed")]
    ProviderMismatch,
    #[error("Invalid model/list params")]
    InvalidParams,
    #[error("Model catalog is pending")]
    Pending,
    #[error("Model catalog is unavailable")]
    Unavailable,
}

/// Shared catalog surface used by rename and the HTTP model route.
pub trait ModelListPort: Send + Sync {
    fn list<'a>(
        &'a self,
        account_key: &'a str,
        provider: &'a str,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ModelListError>> + Send + 'a>>;
}

impl ModelListPort for ModelCatalogService {
    fn list<'a>(
        &'a self,
        account_key: &'a str,
        provider: &'a str,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ModelListError>> + Send + 'a>> {
        Box::pin(self.list_models(account_key, provider, params))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CatalogRead {
    Ready(Value),
    Pending,
    Unavailable,
}

async fn read_account_session_catalog(
    session: Arc<dyn crate::account_session_runtime::AccountSessionPort>,
    account: String,
    provider: String,
    stop: Shutdown,
) -> Result<Value, ()> {
    if !matches!(provider.as_str(), "codex" | "claude") {
        return Err(());
    }
    let mut shutdown = stop.subscribe();
    let mut rows = Vec::new();
    // A refresh is a new durable provider read. Page retries within this refresh
    // keep their identity; a later refresh must not replay an expired receipt.
    static REFRESH_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let refresh_id = format!(
        "{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ())?
            .as_nanos(),
        REFRESH_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut first_page = None;
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    let mut seen_models = std::collections::HashSet::new();
    let mut total_bytes = 0_usize;
    for page_index in 0..MAX_CATALOG_PAGES {
        let mut params = json!({
            "limit": MODEL_CATALOG_ITEMS_PER_PAGE,
            "includeHidden": true
        });
        if let Some(cursor) = &cursor {
            params
                .as_object_mut()
                .ok_or(())?
                .insert("cursor".into(), Value::String(cursor.clone()));
        }
        let session_clone = session.clone();
        let account_clone = account.clone();
        // An idle or not-yet-started account worker has no scope. Start it
        // (bounded) instead of reporting the catalog unavailable.
        let current_scope = match session.current_scope(&account) {
            Some(scope) => scope,
            None => tokio::select! {
                _ = Shutdown::cancelled(&mut shutdown) => return Err(()),
                result = tokio::time::timeout(CATALOG_TIMEOUT, session.ensure_session(&account)) => {
                    result.map_err(|_| ())?.map_err(|_| ())?
                }
            },
        };
        let operation = format!(
            "model-list:{account}:{refresh_id}:{page_index}:{}",
            cursor.as_deref().unwrap_or("first")
        );
        use crate::account_session_runtime::{ProviderRequest, ProviderResponse, RequestCertainty};
        let request = session_clone.provider_request(ProviderRequest {
            account: account_clone,
            operation: operation.clone(),
            actor: None,
            deleted_actor_session: None,
            expected_scope: Some(current_scope.clone()),
            method: studio_runtime_providers::Method::ModelList,
            params,
            deadline: std::time::Instant::now() + CATALOG_TIMEOUT,
        });
        let response = tokio::select! {
            _ = Shutdown::cancelled(&mut shutdown) => return Err(()),
            result = tokio::time::timeout(CATALOG_TIMEOUT, request) => {
                let receipt = result.map_err(|_| ())?.map_err(|_| ())?;
                if receipt.scope != current_scope
                    || receipt.operation != operation
                    || receipt.certainty != RequestCertainty::Written
                    || receipt.ticket.is_none()
                {
                    return Err(());
                }
                match receipt.result {
                    Some(ProviderResponse::Reply(value)) => value,
                    Some(ProviderResponse::Rejected(_)) | None => return Err(()),
                }
            },
        };
        let bytes = serde_json::to_vec(&response).map_err(|_| ())?.len();
        if bytes > MAX_CATALOG_PAGE_BYTES {
            return Err(());
        }
        total_bytes = total_bytes.checked_add(bytes).ok_or(())?;
        if total_bytes > MAX_CATALOG_RESPONSE_BYTES {
            return Err(());
        }
        let object = response.as_object().ok_or(())?;
        let data = object.get("data").and_then(Value::as_array).ok_or(())?;
        if first_page.is_none() {
            first_page = Some(object.clone());
        }
        for row in data {
            let model = row
                .as_object()
                .and_then(|row| row.get("model"))
                .and_then(Value::as_str)
                .filter(|model| !model.trim().is_empty())
                .ok_or(())?;
            if !seen_models.insert(model.to_owned()) {
                return Err(());
            }
            rows.push(row.clone());
        }
        match object.get("nextCursor") {
            None | Some(Value::Null) => {
                let mut result = first_page.ok_or(())?;
                result.insert("data".into(), Value::Array(rows));
                result.insert("nextCursor".into(), Value::Null);
                return Ok(Value::Object(result));
            }
            Some(Value::String(value))
                if !value.is_empty() && seen_cursors.insert(value.clone()) =>
            {
                cursor = Some(value.clone())
            }
            _ => return Err(()),
        }
    }
    Err(())
}

/// Admission reads never expose the display cache's stale fallback. The caller
/// must retain account/home/provider identity and recheck expiry before freezing.
#[derive(Clone, Debug, PartialEq)]
pub enum AdmissionCatalogRead {
    Fresh {
        value: Value,
        account_key: String,
        home: String,
        provider: String,
        generation: u64,
        expires_at: std::time::Instant,
    },
    Pending,
    Unavailable,
}

struct CacheEntry {
    generation: u64,
    value: Option<Value>,
    expires_at: Option<Instant>,
    error: bool,
    pending: bool,
}

struct CatalogInner {
    executable: Option<PathBuf>,
    stderr_log: Option<OperationalLog>,
    #[cfg(test)]
    refresh_factory: Mutex<Option<Arc<dyn ProviderTaskFactory>>>,
    cache: Mutex<HashMap<(String, String, String), CacheEntry>>,
    accounts: Mutex<HashMap<String, (String, String)>>,
    accounts_initialized: AtomicBool,
    account_session: Mutex<Option<Arc<dyn crate::account_session_runtime::AccountSessionPort>>>,
    next_generation: Mutex<u64>,
    on_change: ChangeCallback,
    refresh_changed: tokio::sync::Notify,
    tasks: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    cleanup_error: Arc<Mutex<Option<String>>>,
    stop: Shutdown,
}

/// A single HTTP-state-owned cache and its provider process configuration.
#[derive(Clone)]
pub struct ModelCatalogService(Arc<CatalogInner>);

impl ModelCatalogService {
    /// Observe only an already refreshed catalog. This never starts a provider;
    /// use `read` outside admission to request/coalesce the asynchronous refresh.
    #[must_use]
    pub fn read_for_admission(
        &self,
        account_key: &str,
        home: &str,
        provider: &str,
    ) -> AdmissionCatalogRead {
        let key = (account_key.to_owned(), home.to_owned(), provider.to_owned());
        if !matches!(provider, "codex" | "claude") {
            return AdmissionCatalogRead::Unavailable;
        }
        let Ok(cache) = self.0.cache.lock() else {
            return AdmissionCatalogRead::Unavailable;
        };
        let Some(entry) = cache.get(&key) else {
            return AdmissionCatalogRead::Pending;
        };
        if entry.pending {
            return AdmissionCatalogRead::Pending;
        }
        if entry.error {
            return AdmissionCatalogRead::Unavailable;
        }
        match (&entry.value, entry.expires_at) {
            (Some(value), Some(expires_at)) if expires_at > Instant::now() => {
                AdmissionCatalogRead::Fresh {
                    value: value.clone(),
                    account_key: key.0,
                    home: key.1,
                    provider: key.2,
                    generation: entry.generation,
                    expires_at: expires_at.into_std(),
                }
            }
            _ => AdmissionCatalogRead::Pending,
        }
    }

    /// Start/coalesce the async refresh and wait for its published result at a
    /// point of need, outside the worker startup and synchronous store section.
    pub async fn wait_for_admission_catalog(
        &self,
        account_key: &str,
        home: &str,
        provider: &str,
    ) -> AdmissionCatalogRead {
        let deadline = Instant::now() + CATALOG_ADMISSION_WAIT;
        if tokio::time::timeout_at(deadline, self.read(account_key, home, provider, false))
            .await
            .is_err()
        {
            return AdmissionCatalogRead::Pending;
        }
        loop {
            let notified = self.0.refresh_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let result = self.read_for_admission(account_key, home, provider);
            if !matches!(result, AdmissionCatalogRead::Pending) {
                return result;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return AdmissionCatalogRead::Pending;
            }
        }
    }

    /// Wait until admission can make progress on this catalog: start or join a
    /// refresh now and wait for a published result; after a cached failure,
    /// wait for its retry deadline (or any earlier published result).
    pub async fn refreshed(&self, account_key: &str, home: &str, provider: &str) {
        let notified = self.0.refresh_changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let _status = self.read(account_key, home, provider, false).await;
        if matches!(
            self.read_for_admission(account_key, home, provider),
            AdmissionCatalogRead::Fresh { .. }
        ) {
            return;
        }
        let key = (account_key.to_owned(), home.to_owned(), provider.to_owned());
        let retry_at = self.0.cache.lock().ok().and_then(|cache| {
            cache
                .get(&key)
                .filter(|entry| entry.error && !entry.pending)
                .and_then(|entry| entry.expires_at)
        });
        match retry_at {
            Some(at) => {
                tokio::select! { () = &mut notified => {}, () = tokio::time::sleep_until(at) => {} }
            }
            None => notified.await,
        }
    }

    #[must_use]
    pub fn new(executable: Option<String>) -> Self {
        Self::with_change_callback(executable, Arc::new(|| {}))
    }

    /// Construct the cache with a shared resource-change callback for completed reads.
    #[must_use]
    pub fn with_change_callback(executable: Option<String>, on_change: ChangeCallback) -> Self {
        Self::with_change_callback_and_log(executable, on_change, None)
    }

    /// Construct the cache with the state-directory provider stderr writer.
    #[must_use]
    pub fn with_change_callback_and_log(
        executable: Option<String>,
        on_change: ChangeCallback,
        stderr_log: Option<OperationalLog>,
    ) -> Self {
        Self(Arc::new(CatalogInner {
            executable: executable.map(PathBuf::from),
            stderr_log,
            #[cfg(test)]
            refresh_factory: Mutex::new(None),
            cache: Mutex::new(HashMap::new()),
            accounts: Mutex::new(HashMap::new()),
            accounts_initialized: AtomicBool::new(false),
            account_session: Mutex::new(None),
            next_generation: Mutex::new(0),
            on_change,
            refresh_changed: tokio::sync::Notify::new(),
            tasks: tokio::sync::Mutex::new(Vec::new()),
            cleanup_error: Arc::default(),
            stop: Shutdown::default(),
        }))
    }

    /// Display reads return stale data immediately while one refresh runs.
    pub async fn read(
        &self,
        account_key: &str,
        home: &str,
        provider: &str,
        retry: bool,
    ) -> CatalogRead {
        if !matches!(provider, "codex" | "claude")
            || *self.0.stop.subscribe().borrow()
            || (provider == "claude"
                && self
                    .0
                    .account_session
                    .lock()
                    .ok()
                    .is_none_or(|session| session.is_none()))
        {
            return CatalogRead::Unavailable;
        }

        let cache_key = (account_key.to_owned(), home.to_owned(), provider.to_owned());
        // Own task-slot registration before marking the cache pending. If this
        // future is cancelled while waiting for the registry, no orphaned
        // pending entry can suppress future refresh attempts.
        loop {
            let finished = {
                let mut tasks = self.0.tasks.lock().await;
                if *self.0.stop.subscribe().borrow() {
                    return CatalogRead::Unavailable;
                }
                tasks
                    .iter()
                    .position(tokio::task::JoinHandle::is_finished)
                    .map(|index| tasks.swap_remove(index))
            };
            let Some(mut task) = finished else {
                break;
            };
            if let Err(error) = (&mut task).await {
                *self.0.cleanup_error.lock().expect("catalog cleanup report") =
                    Some(error.to_string());
            }
            drop(task);
        }
        // Acquire the task registry before reserving the cache entry. Once
        // Pending is published below, task registration is synchronous while
        // this guard is held, so cancellation cannot strand an unowned refresh.
        let mut tasks = self.0.tasks.lock().await;
        if *self.0.stop.subscribe().borrow() {
            return CatalogRead::Unavailable;
        }
        // Keep the retained identity stable while reserving a refresh. A route
        // that captured an old account snapshot must not restart its old catalog
        // after retain_accounts has rotated the account home or provider.
        let profiles = match self.0.accounts.lock() {
            Ok(profiles) => profiles,
            Err(_) => return CatalogRead::Unavailable,
        };
        if self.0.accounts_initialized.load(Ordering::Acquire)
            && !profiles
                .get(account_key)
                .is_some_and(|identity| identity.0 == home && identity.1 == provider)
        {
            return CatalogRead::Unavailable;
        }
        // Registry acquisition and completed-task cleanup can suspend. Sample
        // freshness only after those waits so an entry cannot expire while this
        // read is queued and still be returned as fresh.
        let now = Instant::now();
        let (generation, stale, permit) = {
            let Ok(mut cache) = self.0.cache.lock() else {
                return CatalogRead::Unavailable;
            };
            if let Some(entry) = cache.get_mut(&cache_key) {
                if entry.pending {
                    return match &entry.value {
                        Some(value) => CatalogRead::Ready(value.clone()),
                        None => CatalogRead::Pending,
                    };
                }
                if entry.expires_at.is_some_and(|expires| expires > now) {
                    if entry.error {
                        if !retry {
                            return CatalogRead::Unavailable;
                        }
                    } else if let Some(value) = &entry.value {
                        return CatalogRead::Ready(value.clone());
                    }
                }
            }
            let stale = cache.get(&cache_key).and_then(|entry| entry.value.clone());
            let Some(permit) = provider_exchange::try_admit() else {
                return stale.map_or(CatalogRead::Pending, CatalogRead::Ready);
            };
            // A changed home/provider replaces the same account's former entry.
            cache.retain(|key, _| key.0 != account_key || *key == cache_key);
            let Ok(mut next) = self.0.next_generation.lock() else {
                return CatalogRead::Unavailable;
            };
            *next = next.saturating_add(1);
            let generation = *next;
            cache.insert(
                cache_key.clone(),
                CacheEntry {
                    generation,
                    value: stale.clone(),
                    expires_at: None,
                    error: false,
                    pending: true,
                },
            );
            (generation, stale, permit)
        };

        let service = Arc::downgrade(&self.0);
        let executable = self.0.executable.clone();
        let account_session = self
            .0
            .account_session
            .lock()
            .ok()
            .and_then(|session| session.clone());
        let provider = provider.to_owned();
        let stop = self.0.stop.clone();
        let account = account_key.to_owned();
        let home = home.to_owned();
        let cleanup_error = self.0.cleanup_error.clone();
        let stderr_log = self.0.stderr_log.clone();
        #[cfg(test)]
        let refresh_factory = self
            .0
            .refresh_factory
            .lock()
            .expect("fixture factory")
            .clone();
        tasks.push(tokio::spawn(async move {
            #[cfg(test)]
            if let Some(factory) = refresh_factory {
                let _permit = permit;
                let result = read_session_with_shutdown(
                    factory,
                    ProviderLaunchSpec::new(
                        vec!["fixture".into()],
                        Default::default(),
                        std::env::temp_dir(),
                    ),
                    Instant::now(),
                    stop,
                    cleanup_error,
                )
                .await;
                if let Some(inner) = service.upgrade() {
                    store_result(&inner, &cache_key, generation, result);
                }
                return;
            }
            let result = if let Some(session) = account_session {
                read_account_session_catalog(session, account.clone(), provider, stop).await
            } else if provider == "claude" {
                Err(())
            } else {
                read_provider_catalog(
                    permit,
                    executable.as_deref(),
                    &home,
                    account != "default",
                    stop,
                    cleanup_error,
                    stderr_log,
                )
                .await
            };
            if let Some(inner) = service.upgrade() {
                store_result(&inner, &cache_key, generation, result);
            }
        }));
        stale.map_or(CatalogRead::Pending, CatalogRead::Ready)
    }

    /// Wait for a cold display read to publish, while returning stale display
    /// data immediately when the cache already has a value. A failed refresh
    /// serves the last known catalog for display; admission never does.
    pub async fn read_until_ready(
        &self,
        account_key: &str,
        home: &str,
        provider: &str,
        retry: bool,
    ) -> CatalogRead {
        match self
            .wait_until_ready(account_key, home, provider, retry)
            .await
        {
            CatalogRead::Unavailable => self
                .last_known(account_key, home, provider)
                .map_or(CatalogRead::Unavailable, CatalogRead::Ready),
            read => read,
        }
    }

    fn last_known(&self, account_key: &str, home: &str, provider: &str) -> Option<Value> {
        if !self.account_identity_is_current(account_key, home, provider) {
            return None;
        }
        let key = (account_key.to_owned(), home.to_owned(), provider.to_owned());
        self.0.cache.lock().ok()?.get(&key)?.value.clone()
    }

    async fn wait_until_ready(
        &self,
        account_key: &str,
        home: &str,
        provider: &str,
        retry: bool,
    ) -> CatalogRead {
        let deadline = Instant::now() + CATALOG_TIMEOUT;
        match tokio::time::timeout_at(deadline, self.read(account_key, home, provider, retry)).await
        {
            Ok(CatalogRead::Ready(value)) => {
                return if self.account_identity_is_current(account_key, home, provider) {
                    CatalogRead::Ready(value)
                } else {
                    CatalogRead::Unavailable
                };
            }
            Ok(CatalogRead::Unavailable) => return CatalogRead::Unavailable,
            Ok(CatalogRead::Pending) => {}
            Err(_) => return CatalogRead::Pending,
        }

        loop {
            let changed = self.0.refresh_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match tokio::time::timeout_at(deadline, self.read(account_key, home, provider, false))
                .await
            {
                Ok(CatalogRead::Ready(value)) => {
                    return if self.account_identity_is_current(account_key, home, provider) {
                        CatalogRead::Ready(value)
                    } else {
                        CatalogRead::Unavailable
                    };
                }
                Ok(CatalogRead::Unavailable) => return CatalogRead::Unavailable,
                Ok(CatalogRead::Pending) => {}
                Err(_) => return CatalogRead::Pending,
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return CatalogRead::Pending;
            }
        }
    }

    fn account_identity_is_current(&self, account_key: &str, home: &str, provider: &str) -> bool {
        if !self.0.accounts_initialized.load(Ordering::Acquire) {
            return true;
        }
        self.0.accounts.lock().ok().is_some_and(|profiles| {
            profiles
                .get(account_key)
                .is_some_and(|identity| identity.0 == home && identity.1 == provider)
        })
    }

    /// Start or join a catalog refresh and wait for its completion notification.
    /// Admission paths use this before durable provider effects so they never
    /// create a child from stale or pending model metadata.
    pub async fn read_until_fresh(
        &self,
        account_key: &str,
        home: &str,
        provider: &str,
    ) -> CatalogRead {
        if provider != "codex" {
            return CatalogRead::Unavailable;
        }
        let wait_for_result = async {
            loop {
                if let AdmissionCatalogRead::Fresh { value, .. } =
                    self.read_for_admission(account_key, home, provider)
                {
                    return CatalogRead::Ready(value);
                }
                let changed = self.0.refresh_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                match self.read(account_key, home, provider, false).await {
                    CatalogRead::Unavailable => return CatalogRead::Unavailable,
                    CatalogRead::Pending | CatalogRead::Ready(_) => {
                        if let AdmissionCatalogRead::Fresh { value, .. } =
                            self.read_for_admission(account_key, home, provider)
                        {
                            return CatalogRead::Ready(value);
                        }
                        changed.await;
                    }
                }
            }
        };
        tokio::time::timeout(CATALOG_TIMEOUT, wait_for_result)
            .await
            .unwrap_or(CatalogRead::Pending)
    }
    pub fn retain_accounts(&self, accounts: &[Value]) {
        if let Ok(mut profiles) = self.0.accounts.lock() {
            profiles.clear();
            profiles.extend(accounts.iter().filter_map(|account| {
                let id = account.get("id")?.as_str()?.to_owned();
                let home = account
                    .get("home")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let provider = studio_runtime_providers::ProviderKind::of_record(account)
                    .ok()?
                    .key()
                    .to_owned();
                Some((id, (home, provider)))
            }));
            self.0.accounts_initialized.store(true, Ordering::Release);
        }
        self.0
            .cache
            .lock()
            .expect("catalog cache")
            .retain(|key, _| {
                accounts.iter().any(|account| {
                    account["id"].as_str() == Some(key.0.as_str())
                        && account["home"].as_str().unwrap_or("") == key.1
                        && studio_runtime_providers::ProviderKind::key_of(account)
                            == Some(key.2.as_str())
                })
            });
        self.0.refresh_changed.notify_waiters();
    }

    /// Return the registered provider/home identity for transfer validation.
    #[must_use]
    pub fn account_profile(&self, account: &str) -> Option<(String, String)> {
        self.0.accounts.lock().ok()?.get(account).cloned()
    }
    /// Attach the account registry's retained provider session. Model reads
    /// share the live account connection instead of launching a second provider.
    pub fn set_account_session(
        &self,
        session: Option<Arc<dyn crate::account_session_runtime::AccountSessionPort>>,
    ) {
        if let Ok(mut current) = self.0.account_session.lock() {
            *current = session;
        }
    }

    #[cfg(test)]
    pub(crate) fn seed_test_admission_catalog(
        &self,
        account_key: &str,
        home: &str,
        provider: &str,
        value: Value,
    ) {
        self.0.cache.lock().expect("catalog cache").insert(
            (account_key.to_owned(), home.to_owned(), provider.to_owned()),
            CacheEntry {
                generation: 1,
                value: Some(value),
                expires_at: Some(Instant::now() + Duration::from_secs(30)),
                error: false,
                pending: false,
            },
        );
    }

    pub fn model_list_port(&self) -> Arc<dyn ModelListPort> {
        Arc::new(self.clone())
    }

    async fn list_models(
        &self,
        account: &str,
        provider: &str,
        params: Value,
    ) -> Result<Value, ModelListError> {
        if params != json!({"limit": MODEL_CATALOG_ITEMS_PER_PAGE}) {
            return Err(ModelListError::InvalidParams);
        }
        let (home, actual_provider) = self
            .0
            .accounts
            .lock()
            .map_err(|_| ModelListError::Unavailable)?
            .get(account)
            .cloned()
            .ok_or(ModelListError::UnknownAccount)?;
        if actual_provider != provider {
            return Err(ModelListError::ProviderMismatch);
        }
        match self.read(account, &home, provider, false).await {
            CatalogRead::Ready(value) => Ok(value),
            CatalogRead::Pending => Err(ModelListError::Pending),
            CatalogRead::Unavailable => Err(ModelListError::Unavailable),
        }
    }
    pub async fn shutdown(&self) -> std::io::Result<()> {
        self.0.stop.cancel();
        // Handles remain registry-owned while awaiting: cancelling this drain
        // cannot detach unfinished cleanup, and the next shutdown resumes it.
        let mut tasks = self.0.tasks.lock().await;
        while let Some(task) = tasks.last_mut() {
            if let Err(error) = task.await {
                *self.0.cleanup_error.lock().expect("catalog cleanup report") =
                    Some(error.to_string());
            }
            drop(tasks.pop());
        }
        match self
            .0
            .cleanup_error
            .lock()
            .expect("catalog cleanup report")
            .as_ref()
        {
            Some(error) => Err(std::io::Error::other(error.clone())),
            None => Ok(()),
        }
    }
}

impl Drop for CatalogInner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn store_result(
    inner: &CatalogInner,
    key: &(String, String, String),
    generation: u64,
    result: Result<Value, ()>,
) -> bool {
    let stored = if let Ok(mut cache) = inner.cache.lock()
        && let Some(entry) = cache.get_mut(key)
        && entry.generation == generation
        && entry.pending
    {
        entry.pending = false;
        match result {
            Ok(value) => {
                entry.expires_at = Some(Instant::now() + CATALOG_TTL);
                entry.value = Some(value);
                entry.error = false;
            }
            Err(()) => {
                entry.expires_at = Some(Instant::now() + CATALOG_FAILURE_RETRY);
                entry.error = true;
            }
        }
        true
    } else {
        false
    };
    if stored {
        (inner.on_change)();
        inner.refresh_changed.notify_waiters();
    }
    stored
}

async fn read_provider_catalog(
    admission: provider_exchange::ExchangeAdmission,
    executable: Option<&std::path::Path>,
    home: &str,
    isolated: bool,
    stop: Shutdown,
    cleanup_error: Arc<Mutex<Option<String>>>,
    stderr_log: Option<OperationalLog>,
) -> Result<Value, ()> {
    provider_exchange::run_admitted(
        admission,
        executable,
        home,
        isolated,
        provider_exchange::ExchangeSpec {
            cleanup_report: Some(cleanup_error),
            stderr_log,
            timeout: CATALOG_TIMEOUT,
            shutdown: stop,
            initialize_operation: OperationId("catalog-initialize".into()),
            initialize_params: studio_runtime_providers::initialize_params(
                studio_runtime_providers::ClientName::Catalog,
            ),
        },
        |session, started| Box::pin(read_pages(session, started)),
    )
    .await
}

#[cfg(test)]
async fn read_session(
    factory: Arc<dyn ProviderTaskFactory>,
    launch: ProviderLaunchSpec,
    started: Instant,
) -> Result<Value, ()> {
    read_session_with_shutdown(
        factory,
        launch,
        started,
        Shutdown::default(),
        Arc::default(),
    )
    .await
}
#[cfg(test)]
async fn read_session_with_shutdown(
    factory: Arc<dyn ProviderTaskFactory>,
    launch: ProviderLaunchSpec,
    started: Instant,
    shutdown: Shutdown,
    cleanup_error: Arc<Mutex<Option<String>>>,
) -> Result<Value, ()> {
    provider_exchange::run_with_factory(
        factory,
        launch,
        started,
        provider_exchange::ExchangeSpec {
            cleanup_report: Some(cleanup_error),
            stderr_log: None,
            timeout: CATALOG_TIMEOUT,
            shutdown,
            initialize_operation: OperationId("catalog-initialize".into()),
            initialize_params: studio_runtime_providers::initialize_params(
                studio_runtime_providers::ClientName::Catalog,
            ),
        },
        |session, started| Box::pin(read_pages(session, started)),
    )
    .await
}

async fn next_event(
    session: &mut SessionHandle,
) -> Result<crate::provider_session_task::SessionEvent, ()> {
    let event = session.events.recv().await.ok_or(())?;
    settle_catalog_event(session, event).await
}

#[cfg(test)]
tokio::task_local! { static SETTLEMENT_GATE: std::sync::Arc<(tokio::sync::Notify,tokio::sync::Notify)>; }
async fn settle_catalog_event(
    session: &mut SessionHandle,
    event: crate::provider_session_task::SessionEvent,
) -> Result<crate::provider_session_task::SessionEvent, ()> {
    #[cfg(test)]
    if let Ok(gate) = SETTLEMENT_GATE.try_with(Arc::clone) {
        gate.0.notify_one();
        gate.1.notified().await;
    }
    // This ephemeral reader still owns the taken event throughout settlement.
    if let Some(journal) = &event.journal {
        session
            .control
            .settled(journal.cursor)
            .await
            .map_err(|_| ())?;
    }
    Ok(event)
}

async fn catalog_drive<T>(
    session: &mut SessionHandle,
    future: impl std::future::Future<Output = Result<T, SessionTaskError>>,
    pending: &mut std::collections::VecDeque<crate::provider_session_task::SessionEvent>,
) -> Result<T, ()> {
    tokio::pin!(future);
    loop {
        tokio::select! { biased;
            result = &mut future => return result.map_err(|_|()),
            event = session.events.recv() => {
                // recv is cancellation safe; settlement owns the event only
                // after this branch wins, so a command cannot drop its outcomes.
                let event = settle_catalog_event(session, event.ok_or(())?).await?;
                if !event.outcomes.is_empty() {
                    if pending.len() == crate::provider_session_task::MAX_UNSETTLED { return Err(()); }
                    pending.push_back(event);
                }
            }
        }
    }
}

async fn read_pages(session: &mut SessionHandle, started: Instant) -> Result<Value, ()> {
    while session.status.borrow().initialization != InitializationStatus::Ready {
        tokio::select! {
            result = session.status.changed() => result.map_err(|_| ())?,
            event = session.events.recv() => {
                let event = event.ok_or(())?;
                if let Some(journal) = &event.journal { session.control.settled(journal.cursor).await.map_err(|_| ())?; }
                if event.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::GenerationClosed { .. })) { return Err(()); }
            }
        }
    }
    let mut pending = std::collections::VecDeque::new();
    let mut rows = Vec::new();
    let mut first_page = None;
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    let mut seen_models = std::collections::HashSet::new();
    let mut total_bytes = 0_usize;

    loop {
        if cursor.is_some() && seen_cursors.len() >= MAX_CATALOG_PAGES {
            return Err(());
        }
        let remaining = CATALOG_TIMEOUT.checked_sub(started.elapsed()).ok_or(())?;
        let timeout_ms = duration_ms(remaining);
        let params = match &cursor {
            Some(cursor) => {
                json!({"limit": MODEL_CATALOG_ITEMS_PER_PAGE, "includeHidden": true, "cursor": cursor})
            }
            None => json!({"limit": MODEL_CATALOG_ITEMS_PER_PAGE, "includeHidden": true}),
        };
        let control = session.control.clone();
        let prepared = catalog_drive(
            session,
            control.prepare(RequestSpec {
                id: RequestId::String(format!("catalog-page-{}", seen_cursors.len())),
                operation_id: OperationId(format!("catalog-page-{}", seen_cursors.len())),
                method: Method::ModelList,
                params,
                thread_id: None,
                lightweight: true,
                timeout_ms,
            }),
            &mut pending,
        )
        .await?;
        let ticket = catalog_drive(session, control.submit(prepared), &mut pending).await?;
        let page = 'response: loop {
            let event = match pending.pop_front() {
                Some(event) => event,
                None => next_event(session).await?,
            };
            for outcome in event.outcomes {
                match outcome {
                    SessionOutcome::Resolved {
                        ticket: received,
                        outcome:
                            RequestOutcome::Reply {
                                body: ResponseBody::Result(value),
                                late: false,
                            },
                    } if received == ticket => break 'response value,
                    SessionOutcome::Resolved {
                        ticket: received, ..
                    } if received == ticket => return Err(()),
                    SessionOutcome::Write {
                        ticket: Some(received),
                        outcome: WriteOutcome::NotSent | WriteOutcome::Unknown,
                        ..
                    } if received == ticket => return Err(()),
                    SessionOutcome::GenerationClosed { .. } => return Err(()),
                    _ => {}
                }
            }
        };
        let page_bytes = serde_json::to_vec(&page).map_err(|_| ())?.len();
        if page_bytes > MAX_CATALOG_PAGE_BYTES {
            return Err(());
        }
        total_bytes = total_bytes.checked_add(page_bytes).ok_or(())?;
        if total_bytes > MAX_CATALOG_RESPONSE_BYTES {
            return Err(());
        }
        let page_object = page.as_object().ok_or(())?;
        let data = page_object
            .get("data")
            .and_then(Value::as_array)
            .ok_or(())?;
        if first_page.is_none() {
            first_page = Some(page_object.clone());
        }
        for row in data {
            let model = row
                .as_object()
                .and_then(|row| row.get("model"))
                .and_then(Value::as_str)
                .filter(|model| !model.trim().is_empty())
                .ok_or(())?;
            if !seen_models.insert(model.to_owned()) {
                return Err(());
            }
            rows.push(row.clone());
        }
        match page_object.get("nextCursor") {
            None | Some(Value::Null) => break,
            Some(Value::String(value))
                if !value.is_empty() && seen_cursors.insert(value.clone()) =>
            {
                cursor = Some(value.clone());
            }
            _ => return Err(()),
        }
    }

    let mut result = first_page.ok_or(())?;
    result.insert("data".to_owned(), Value::Array(rows));
    result.insert("nextCursor".to_owned(), Value::Null);
    Ok(Value::Object(result))
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::provider_session_task::SessionTaskConfig;
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;
    use studio_runtime_process::ProviderLaunchSpec;
    use studio_runtime_process::{EventCursor, provider_seam::*};
    use studio_runtime_providers::Frame;
    use tokio::sync::{mpsc, watch};

    struct AccountSessionPages {
        pages: StdMutex<Vec<Value>>,
        params: StdMutex<Vec<Value>>,
        operations: StdMutex<Vec<String>>,
    }
    impl crate::account_session_runtime::AccountSessionPort for AccountSessionPages {
        fn current_scope(
            &self,
            account: &str,
        ) -> Option<crate::account_session_runtime::ConnectionScope> {
            Some(crate::account_session_runtime::ConnectionScope {
                account: account.to_owned(),
                connection: "claude-connection".into(),
                generation: 4,
            })
        }

        fn provider_request<'a>(
            &'a self,
            request: crate::account_session_runtime::ProviderRequest,
        ) -> crate::account_session_runtime::AccountSessionFuture<
            'a,
            crate::account_session_runtime::SessionReceipt,
        > {
            Box::pin(async move {
                use crate::account_session_runtime::{
                    ProviderResponse, RequestCertainty, SessionReceipt,
                };
                self.params.lock().expect("params").push(request.params);
                self.operations
                    .lock()
                    .expect("operations")
                    .push(request.operation.clone());
                if self.pages.lock().expect("pages").is_empty() {
                    return Err(
                        crate::account_session_runtime::AccountSessionError::InvalidResponse(
                            "/".into(),
                        ),
                    );
                }
                let scope = self.current_scope(&request.account).expect("test scope");
                Ok(SessionReceipt {
                    scope,
                    operation: request.operation,
                    ticket: Some(studio_runtime_providers::RequestTicket {
                        id: studio_runtime_providers::RequestId::String("catalog-test".into()),
                        generation: 4,
                        method: studio_runtime_providers::Method::ModelList,
                    }),
                    certainty: RequestCertainty::Written,
                    result: Some(ProviderResponse::Reply(
                        self.pages.lock().expect("pages").remove(0),
                    )),
                    not_sent_reason: None,
                    replayed: false,
                })
            })
        }
    }

    #[tokio::test]
    async fn claude_catalog_uses_retained_session_and_paginates_python_params() {
        let session = Arc::new(AccountSessionPages {
            pages: StdMutex::new(vec![
                json!({"data":[{"model":"one"}],"nextCursor":"next","metadata":true}),
                json!({"data":[{"model":"two"}],"nextCursor":null}),
            ]),
            params: StdMutex::new(Vec::new()),
            operations: StdMutex::new(Vec::new()),
        });
        let result = read_account_session_catalog(
            session.clone(),
            "account".into(),
            "claude".into(),
            Shutdown::default(),
        )
        .await
        .expect("catalog pages");
        assert_eq!(
            result,
            json!({
                "data":[{"model":"one"},{"model":"two"}],
                "nextCursor":null,
                "metadata":true
            })
        );
        assert_eq!(
            session.params.lock().expect("params").as_slice(),
            &[
                json!({"limit":MODEL_CATALOG_ITEMS_PER_PAGE,"includeHidden":true}),
                json!({"limit":MODEL_CATALOG_ITEMS_PER_PAGE,"includeHidden":true,"cursor":"next"}),
            ]
        );
    }

    /// Idle worker: no scope until `ensure_session` starts it.
    struct LazySession {
        inner: AccountSessionPages,
        started: std::sync::atomic::AtomicBool,
        ensure_fails: bool,
    }
    impl crate::account_session_runtime::AccountSessionPort for LazySession {
        fn current_scope(
            &self,
            account: &str,
        ) -> Option<crate::account_session_runtime::ConnectionScope> {
            if self.started.load(Ordering::SeqCst) {
                self.inner.current_scope(account)
            } else {
                None
            }
        }
        fn ensure_session<'a>(
            &'a self,
            account: &'a str,
        ) -> crate::account_session_runtime::AccountSessionFuture<
            'a,
            crate::account_session_runtime::ConnectionScope,
        > {
            Box::pin(async move {
                if self.ensure_fails {
                    return Err(crate::account_session_runtime::AccountSessionError::Unknown);
                }
                self.started.store(true, Ordering::SeqCst);
                Ok(self.inner.current_scope(account).expect("scope"))
            })
        }
        fn provider_request<'a>(
            &'a self,
            request: crate::account_session_runtime::ProviderRequest,
        ) -> crate::account_session_runtime::AccountSessionFuture<
            'a,
            crate::account_session_runtime::SessionReceipt,
        > {
            self.inner.provider_request(request)
        }
    }

    fn lazy_session(ensure_fails: bool) -> Arc<LazySession> {
        Arc::new(LazySession {
            inner: AccountSessionPages {
                pages: StdMutex::new(vec![json!({"data":[{"model":"gpt-6-luna"}]})]),
                params: StdMutex::new(Vec::new()),
                operations: StdMutex::new(Vec::new()),
            },
            started: std::sync::atomic::AtomicBool::new(false),
            ensure_fails,
        })
    }

    #[tokio::test]
    async fn catalog_refresh_starts_an_idle_account_worker() {
        let session = lazy_session(false);
        let result = read_account_session_catalog(
            session.clone(),
            "codex2".into(),
            "codex".into(),
            Shutdown::default(),
        )
        .await
        .expect("catalog after lazy start");
        assert_eq!(result["data"][0]["model"], "gpt-6-luna");
        assert!(session.started.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn catalog_refresh_reports_unavailable_when_worker_cannot_start() {
        let session = lazy_session(true);
        assert!(
            read_account_session_catalog(
                session,
                "codex2".into(),
                "codex".into(),
                Shutdown::default(),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn each_catalog_refresh_has_a_fresh_durable_operation_identity() {
        let session = Arc::new(AccountSessionPages {
            pages: StdMutex::new(vec![
                json!({"data":[{"model":"one"}]}),
                json!({"data":[{"model":"one"}]}),
            ]),
            params: StdMutex::new(Vec::new()),
            operations: StdMutex::new(Vec::new()),
        });
        for _ in 0..2 {
            read_account_session_catalog(
                session.clone(),
                "account".into(),
                "claude".into(),
                Shutdown::default(),
            )
            .await
            .expect("catalog refresh");
        }
        let operations = session.operations.lock().expect("operations");
        assert_eq!(operations.len(), 2);
        assert_ne!(operations[0], operations[1]);
        assert!(
            operations
                .iter()
                .all(|operation| operation.starts_with("model-list:account:"))
        );
    }

    #[tokio::test]
    async fn shared_model_list_checks_request_and_account_identity() {
        let service = ModelCatalogService::new(None);
        service.retain_accounts(&[json!({
            "id":"claude-account",
            "home":"/home/claude",
            "provider":"claude"
        })]);
        let port = service.model_list_port();
        assert_eq!(
            port.list(
                "missing",
                "claude",
                json!({"limit": MODEL_CATALOG_ITEMS_PER_PAGE})
            )
            .await,
            Err(ModelListError::UnknownAccount)
        );
        assert_eq!(
            port.list(
                "claude-account",
                "codex",
                json!({"limit": MODEL_CATALOG_ITEMS_PER_PAGE})
            )
            .await,
            Err(ModelListError::ProviderMismatch)
        );
        assert_eq!(
            port.list("claude-account", "claude", json!({"cursor":"caller-owned"}))
                .await,
            Err(ModelListError::InvalidParams)
        );
    }

    pub(crate) fn set_refresh_factory(
        service: &ModelCatalogService,
        factory: Arc<dyn ProviderTaskFactory>,
    ) {
        *service.0.refresh_factory.lock().expect("fixture factory") = Some(factory);
    }
    pub(crate) fn expire_fixture(service: &ModelCatalogService) {
        for entry in service.0.cache.lock().expect("cache").values_mut() {
            entry.expires_at = Some(Instant::now());
        }
    }
    pub(crate) fn publish_fixture(
        service: &ModelCatalogService,
        account: &str,
        home: &str,
        value: Value,
    ) {
        let key = (account.to_owned(), home.to_owned(), "codex".to_owned());
        service.0.cache.lock().expect("cache").insert(
            key.clone(),
            CacheEntry {
                generation: 7,
                value: None,
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        assert!(store_result(&service.0, &key, 7, Ok(value)));
    }

    #[tokio::test]
    async fn read_until_fresh_waits_for_catalog_change_notification() {
        let service = ModelCatalogService::new(None);
        let key = ("account".to_owned(), "/home".to_owned(), "codex".to_owned());
        service.0.cache.lock().expect("cache").insert(
            key.clone(),
            CacheEntry {
                generation: 7,
                value: Some(json!({"data":[{"model":"stale"}]})),
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        let waiting = service.clone();
        let read =
            tokio::spawn(
                async move { waiting.read_until_fresh("account", "/home", "codex").await },
            );
        tokio::task::yield_now().await;
        let value = json!({"data":[{"model":"fixture"}]});
        assert!(store_result(&service.0, &key, 7, Ok(value.clone())));
        assert_eq!(
            read.await.expect("catalog waiter"),
            CatalogRead::Ready(value)
        );
    }

    #[tokio::test]
    async fn cold_display_read_waits_for_shared_refresh() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let service = ModelCatalogService::new(None);
        let opens = Arc::new(AtomicUsize::new(0));
        set_refresh_factory(
            &service,
            Arc::new(CountingFactory {
                opens: opens.clone(),
                script: Script::new(vec![json!({"data":[{"model":"fixture"}]})], 0),
            }),
        );

        let (first, second) = tokio::join!(
            service.read_until_ready("account", "/home", "codex", false),
            service.read_until_ready("account", "/home", "codex", false),
        );
        let ready = CatalogRead::Ready(json!({
            "data":[{"model":"fixture"}],
            "nextCursor":null
        }));
        assert_eq!(first, ready);
        assert_eq!(second, ready);
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        service.shutdown().await.expect("catalog task shutdown");
    }

    #[tokio::test]
    async fn cold_display_read_reports_failed_refresh_as_unavailable() {
        let service = ModelCatalogService::new(None);
        let key = ("account".to_owned(), "/home".to_owned(), "codex".to_owned());
        service.0.cache.lock().expect("cache").insert(
            key.clone(),
            CacheEntry {
                generation: 7,
                value: None,
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        let waiting = service.clone();
        let read = tokio::spawn(async move {
            waiting
                .read_until_ready("account", "/home", "codex", false)
                .await
        });
        tokio::task::yield_now().await;
        assert!(store_result(&service.0, &key, 7, Err(())));
        assert_eq!(
            read.await.expect("catalog waiter"),
            CatalogRead::Unavailable
        );
    }

    #[tokio::test]
    async fn failed_refresh_serves_last_known_display_catalog_but_not_admission() {
        let service = ModelCatalogService::new(None);
        let key = ("account".to_owned(), "/home".to_owned(), "codex".to_owned());
        let known = json!({"data":[{"model":"known"}],"nextCursor":null});
        service.0.cache.lock().expect("cache").insert(
            key.clone(),
            CacheEntry {
                generation: 7,
                value: Some(known.clone()),
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        assert!(store_result(&service.0, &key, 7, Err(())));

        assert_eq!(
            service.read_for_admission(&key.0, &key.1, &key.2),
            super::AdmissionCatalogRead::Unavailable
        );
        assert_eq!(
            service.read("account", "/home", "codex", false).await,
            CatalogRead::Unavailable
        );
        assert_eq!(
            service
                .read_until_ready("account", "/home", "codex", false)
                .await,
            CatalogRead::Ready(known)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_refresh_is_retried_after_the_failure_backoff_not_the_ttl() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        assert!(CATALOG_FAILURE_RETRY < CATALOG_TTL);
        let service = ModelCatalogService::new(None);
        let key = ("account".to_owned(), "/home".to_owned(), "codex".to_owned());
        service.0.cache.lock().expect("cache").insert(
            key.clone(),
            CacheEntry {
                generation: 7,
                value: None,
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        assert!(store_result(&service.0, &key, 7, Err(())));
        let opens = Arc::new(AtomicUsize::new(0));
        set_refresh_factory(
            &service,
            Arc::new(CountingFactory {
                opens: opens.clone(),
                script: Script::new(vec![json!({"data":[{"model":"fixture"}]})], 0),
            }),
        );

        assert_eq!(
            service.read("account", "/home", "codex", false).await,
            CatalogRead::Unavailable
        );
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        tokio::time::advance(CATALOG_FAILURE_RETRY).await;
        assert_eq!(
            service
                .read_until_ready("account", "/home", "codex", false)
                .await,
            CatalogRead::Ready(json!({"data":[{"model":"fixture"}],"nextCursor":null}))
        );
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        service.shutdown().await.expect("catalog task shutdown");
    }

    #[tokio::test(start_paused = true)]
    async fn cold_display_read_keeps_its_bounded_pending_result() {
        let service = ModelCatalogService::new(None);
        service.0.cache.lock().expect("cache").insert(
            ("account".to_owned(), "/home".to_owned(), "codex".to_owned()),
            CacheEntry {
                generation: 7,
                value: None,
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        let waiting = service.clone();
        let read = tokio::spawn(async move {
            waiting
                .read_until_ready("account", "/home", "codex", false)
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(CATALOG_TIMEOUT).await;
        assert_eq!(read.await.expect("catalog waiter"), CatalogRead::Pending);
    }

    #[tokio::test]
    async fn stale_display_value_returns_without_waiting_for_refresh() {
        let service = ModelCatalogService::new(None);
        service.0.cache.lock().expect("cache").insert(
            ("account".to_owned(), "/home".to_owned(), "codex".to_owned()),
            CacheEntry {
                generation: 7,
                value: Some(json!({"data":[{"model":"stale"}]})),
                expires_at: None,
                error: false,
                pending: true,
            },
        );

        assert_eq!(
            service
                .read_until_ready("account", "/home", "codex", false)
                .await,
            CatalogRead::Ready(json!({"data":[{"model":"stale"}]}))
        );
    }

    #[tokio::test]
    async fn account_rotation_rejects_waiter_and_does_not_repopulate_old_identity() {
        let service = ModelCatalogService::new(None);
        let key = (
            "account".to_owned(),
            "/old-home".to_owned(),
            "codex".to_owned(),
        );
        service.retain_accounts(&[json!({
            "id":"account",
            "home":"/old-home",
            "provider":"codex"
        })]);
        service.0.cache.lock().expect("cache").insert(
            key.clone(),
            CacheEntry {
                generation: 7,
                value: None,
                expires_at: None,
                error: false,
                pending: true,
            },
        );

        let waiting = service.clone();
        let read = tokio::spawn(async move {
            waiting
                .read_until_ready("account", "/old-home", "codex", false)
                .await
        });
        tokio::task::yield_now().await;
        service.retain_accounts(&[json!({
            "id":"account",
            "home":"/new-home",
            "provider":"claude"
        })]);

        assert_eq!(
            read.await.expect("catalog waiter"),
            CatalogRead::Unavailable
        );
        assert!(!service.0.cache.lock().expect("cache").contains_key(&key));
        assert_eq!(
            service
                .read_until_ready("account", "/old-home", "codex", false)
                .await,
            CatalogRead::Unavailable
        );
        assert!(!service.0.cache.lock().expect("cache").contains_key(&key));
    }

    #[tokio::test]
    async fn cancelled_shutdown_keeps_refresh_join_ownership() {
        let service = ModelCatalogService::new(None);
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        service.0.tasks.lock().await.push(tokio::spawn(async move {
            wait.await.expect("release refresh");
        }));
        let first = service.clone();
        let drain = tokio::spawn(async move { first.shutdown().await });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !*service.0.stop.subscribe().borrow() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drain starts");
        drain.abort();
        assert!(drain.await.expect_err("cancelled drain").is_cancelled());
        assert_eq!(
            service.0.tasks.lock().await.len(),
            1,
            "unfinished refresh remains owned"
        );
        release.send(()).expect("refresh retained");
        service.shutdown().await.expect("resumed drain");
        assert!(service.0.tasks.lock().await.is_empty());
        *service.0.cleanup_error.lock().expect("report") =
            Some("provider child 12345 outlived reap grace; termination is unconfirmed".into());
        let error = service
            .shutdown()
            .await
            .expect_err("cleanup failure remains observable");
        assert!(error.to_string().contains("12345"));
    }

    #[tokio::test]
    async fn cancelled_read_waiting_for_task_registry_does_not_leave_pending_entry() {
        let service = ModelCatalogService::new(None);
        let key = ("account".to_owned(), "/home".to_owned(), "codex".to_owned());
        service.0.cache.lock().expect("cache").insert(
            key.clone(),
            CacheEntry {
                generation: 7,
                value: Some(json!({"models":[{"model":"stale"}]})),
                expires_at: Some(Instant::now()),
                error: false,
                pending: false,
            },
        );
        let task_guard = service.0.tasks.lock().await;
        {
            let read = service.read("account", "/home", "codex", false);
            tokio::pin!(read);
            tokio::select! {
                biased;
                result = &mut read => panic!("read passed the held task registry: {result:?}"),
                () = tokio::task::yield_now() => {},
            }
        }
        drop(task_guard);

        let cache = service.0.cache.lock().expect("catalog cache");
        assert!(!cache[&key].pending);
        assert_eq!(
            cache[&key].value,
            Some(json!({"models":[{"model":"stale"}]}))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn read_rechecks_expiry_after_waiting_for_task_registry() {
        let service = ModelCatalogService::new(None);
        let key = ("account".to_owned(), "/home".to_owned(), "codex".to_owned());
        service.0.cache.lock().expect("cache").insert(
            key.clone(),
            CacheEntry {
                generation: 7,
                value: Some(json!({"models":[{"model":"stale"}]})),
                expires_at: Some(Instant::now() + Duration::from_secs(1)),
                error: false,
                pending: false,
            },
        );
        let task_guard = service.0.tasks.lock().await;
        let read = service.read("account", "/home", "codex", false);
        tokio::pin!(read);
        tokio::select! {
            biased;
            result = &mut read => panic!("read passed the held task registry: {result:?}"),
            () = tokio::task::yield_now() => {},
        }
        tokio::time::advance(Duration::from_secs(2)).await;
        drop(task_guard);

        assert_eq!(
            read.await,
            CatalogRead::Ready(json!({"models":[{"model":"stale"}]})),
        );
        assert!(
            service.0.cache.lock().expect("cache")[&key].pending,
            "expired stale data must have a refresh registered"
        );
        service.shutdown().await.expect("join refresh task");
    }

    #[tokio::test]
    async fn command_receipt_cannot_cancel_a_taken_catalog_event() {
        let factory = Arc::new(Script::new(vec![], 0));
        let mut session = SessionHandle::open(
            factory,
            ProviderHandle::for_account("catalog"),
            OpenOptions {
                clock: SessionClock::default(),
                launch: ProviderLaunchSpec::new(
                    vec!["script".into()],
                    BTreeMap::new(),
                    "/var/tmp".into(),
                ),
                expected_generation: Some(Generation(1)),
            },
            SessionTaskConfig {
                generation: Generation(1),
                claude: false,
                timeout_ms: 30_000,
                initialize_operation: OperationId("initialize".into()),
                request_id_prefix: None,
                recovered: vec![],
                initialize_params: json!({}),
            },
        )
        .await
        .expect("session");
        let (events, receiver) = mpsc::channel(1);
        session.events = receiver;
        events
            .send(crate::provider_session_task::SessionEvent {
                journal: None,
                outcomes: vec![SessionOutcome::GenerationClosed { generation: 7 }],
            })
            .await
            .expect("event");
        let gate = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        let (reply, receipt) = tokio::sync::oneshot::channel::<()>();
        let barrier = gate.clone();
        let task = tokio::spawn(SETTLEMENT_GATE.scope(gate, async move {
            let mut pending = std::collections::VecDeque::new();
            catalog_drive(
                &mut session,
                async { receipt.await.map_err(|_| SessionTaskError::Closed) },
                &mut pending,
            )
            .await
            .expect("command");
            assert_eq!(
                pending.len(),
                1,
                "taken event and its outcomes must survive command completion"
            );
            assert!(matches!(
                pending[0].outcomes[0],
                SessionOutcome::GenerationClosed { generation: 7 }
            ));
            session.shutdown().await;
        }));
        tokio::time::timeout(Duration::from_secs(10), barrier.0.notified())
            .await
            .expect("event taken");
        reply
            .send(())
            .expect("command becomes ready during settlement");
        barrier.1.notify_one();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("settlement deadline")
            .expect("reader");
    }

    struct Script {
        pages: Vec<Value>,
        flood: usize,
        panic_on_exit: bool,
    }

    struct CountingFactory {
        opens: Arc<std::sync::atomic::AtomicUsize>,
        script: Script,
    }

    impl ProviderTaskFactory for CountingFactory {
        fn open(
            &self,
            handle: ProviderHandle,
            options: OpenOptions,
            shutdown: Shutdown,
        ) -> OpenFuture<'_> {
            self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.script.open(handle, options, shutdown)
        }
    }

    impl ProviderTaskFactory for Script {
        fn open(
            &self,
            handle: ProviderHandle,
            options: OpenOptions,
            shutdown: Shutdown,
        ) -> OpenFuture<'_> {
            let pages = self.pages.clone();
            let flood = self.flood;
            let panic_on_exit = self.panic_on_exit;
            Box::pin(async move {
                let (commands, mut inbox) = mpsc::channel::<AdapterCommand>(LANE_CAPACITY);
                let (entries, journal) = mpsc::channel(LANE_CAPACITY);
                let (state, status) = watch::channel(AttachmentStatus {
                    handle,
                    open_attempted: true,
                    info: None,
                    initialization: InitializationStatus::AwaitingReply,
                });
                let mut stop = shutdown.subscribe();
                let task = tokio::spawn(async move {
                    let serve = async {
                        let mut write_index = 0;
                        while let Some(command) = inbox.recv().await {
                            match command {
                                AdapterCommand::Write {
                                    reply, observed, ..
                                } => {
                                    if write_index == 2 {
                                        let burst = async {
                                            for cursor in 1..=flood {
                                                entries
                                                    .send(JournalEntry {
                                                        cursor: EventCursor(cursor as u64),
                                                        generation: Generation(1),
                                                        received_at_ms: options.clock.now_ms(),
                                                        payload: JournalPayload::Other {
                                                            kind: "catalog notice".into(),
                                                            payload: json!({}),
                                                        },
                                                    })
                                                    .await
                                                    .expect("burst journal");
                                            }
                                        };
                                        tokio::pin!(burst);
                                        loop {
                                            tokio::select! {
                                                _ = &mut burst => break,
                                                command = inbox.recv() => match command.expect("burst acknowledgement") {
                                                    AdapterCommand::Ack {cursor,reply} => { reply.send(CommandReceipt::Acknowledged(cursor)).ok().expect("burst ACK"); },
                                                    _ => panic!("catalog must wait for the first write receipt"),
                                                }
                                            }
                                        }
                                    }
                                    let receipt = if write_index == 0 {
                                        CommandReceipt::CachedInitialize(json!({}))
                                    } else {
                                        CommandReceipt::Written {
                                            remote_id: None,
                                            duplicate: false,
                                        }
                                    };
                                    reply.send(receipt).ok().expect("receipt");
                                    observed.await.expect("receipt observed");
                                    if write_index >= 2 {
                                        let index = write_index - 2;
                                        if let Some(page) = pages.get(index) {
                                            entries
                                                .send(JournalEntry {
                                                    cursor: EventCursor(
                                                        flood as u64 + index as u64 + 1,
                                                    ),
                                                    generation: Generation(1),
                                                    received_at_ms: options.clock.now_ms(),
                                                    payload: JournalPayload::Frame(
                                                        Frame::Response {
                                                            id: RequestId::String(format!(
                                                                "catalog-page-{index}"
                                                            )),
                                                            body: ResponseBody::Result(
                                                                page.clone(),
                                                            ),
                                                            fields: Default::default(),
                                                        },
                                                    ),
                                                })
                                                .await
                                                .expect("entry accepted");
                                        }
                                    }
                                    write_index += 1;
                                }
                                AdapterCommand::Ack { cursor, reply } => {
                                    reply
                                        .send(CommandReceipt::Acknowledged(cursor))
                                        .ok()
                                        .expect("ack");
                                }
                            }
                        }
                    };
                    tokio::select! { _ = serve => {}, _ = stop.changed() => {} }
                    if panic_on_exit {
                        panic!("injected provider task join failure");
                    }
                    drop(state);
                    TaskExit {
                        result: Ok(()),
                        cleanup_error: None,
                        recovery_needed: None,
                    }
                });
                Ok(ProviderAttachment::from_task(
                    commands, journal, status, shutdown, task,
                ))
            })
        }
    }
    impl Script {
        fn new(pages: Vec<Value>, flood: usize) -> Self {
            Self {
                pages,
                flood,
                panic_on_exit: false,
            }
        }

        fn panics_on_exit(pages: Vec<Value>) -> Self {
            Self {
                pages,
                flood: 0,
                panic_on_exit: true,
            }
        }
    }
    async fn scripted(pages: Vec<Value>) -> Result<Value, ()> {
        read_session(
            Arc::new(Script::new(pages, 0)),
            ProviderLaunchSpec::new(vec!["scripted".into()], BTreeMap::new(), "/var/tmp".into()),
            Instant::now(),
        )
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn admission_never_exposes_cold_stale_pending_failed_or_foreign_catalogs() {
        use super::AdmissionCatalogRead;
        let service = ModelCatalogService::new(None);
        let key = (
            "account".to_owned(),
            "/disposable".to_owned(),
            "codex".to_owned(),
        );
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, &key.2),
            AdmissionCatalogRead::Pending
        );
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, "claude"),
            AdmissionCatalogRead::Pending
        );
        let value = json!({"data": [{"model": "one"}]});
        let expiry = Instant::now() + CATALOG_TTL;
        service.0.cache.lock().expect("cache").insert(
            key.clone(),
            CacheEntry {
                generation: 7,
                value: Some(value.clone()),
                expires_at: Some(expiry),
                error: false,
                pending: false,
            },
        );
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, &key.2),
            AdmissionCatalogRead::Fresh {
                value: value.clone(),
                account_key: key.0.clone(),
                home: key.1.clone(),
                provider: key.2.clone(),
                generation: 7,
                expires_at: expiry.into_std(),
            }
        );
        assert_eq!(
            service.read_for_admission("foreign", &key.1, &key.2),
            AdmissionCatalogRead::Pending
        );
        assert_eq!(
            service.read_for_admission(&key.0, "/changed-home", &key.2),
            AdmissionCatalogRead::Pending
        );
        tokio::time::advance(CATALOG_TTL).await;
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, &key.2),
            AdmissionCatalogRead::Pending
        );
        {
            let mut cache = service.0.cache.lock().expect("cache");
            let entry = cache.get_mut(&key).expect("entry");
            entry.expires_at = Some(Instant::now() + CATALOG_TTL);
            entry.pending = true;
        }
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, &key.2),
            AdmissionCatalogRead::Pending
        );
        assert!(store_result(&service.0, &key, 7, Err(())));
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, &key.2),
            AdmissionCatalogRead::Unavailable
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pages_preserve_metadata_and_hidden_models() {
        assert_eq!(
            scripted(vec![
                json!({"data":[{"model":"one"}],"nextCursor":"next","metadata":true}),
                json!({"data":[{"model":"two","hidden":true}],"nextCursor":null}),
            ])
            .await,
            Ok(
                json!({"data":[{"model":"one"},{"model":"two","hidden":true}],"nextCursor":null,"metadata":true})
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn malformed_duplicate_and_cyclic_pages_are_refused() {
        for pages in [
            vec![json!({"data":[{}]})],
            vec![json!({"data":[{"model":""}]})],
            vec![json!({"data":[{"model":"same"},{"model":"same"}]})],
            vec![
                json!({"data":[],"nextCursor":"again"}),
                json!({"data":[],"nextCursor":"again"}),
            ],
            vec![json!({"data":[],"nextCursor":2})],
        ] {
            assert_eq!(scripted(pages).await, Err(()));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn missing_reply_uses_the_exchange_deadline() {
        let started = Instant::now();
        let service = ModelCatalogService::new(None);
        let result = read_session_with_shutdown(
            Arc::new(Script::new(vec![], 0)),
            ProviderLaunchSpec::new(vec!["scripted".into()], BTreeMap::new(), "/var/tmp".into()),
            started,
            service.0.stop.clone(),
            service.0.cleanup_error.clone(),
        )
        .await;
        assert_eq!(result, Err(()));
        assert_eq!(started.elapsed(), CATALOG_TIMEOUT);
        assert!(
            service
                .0
                .cleanup_error
                .lock()
                .expect("cleanup report")
                .is_none()
        );
        service
            .shutdown()
            .await
            .expect("request timeout was cleaned up");
    }

    #[tokio::test(start_paused = true)]
    async fn provider_task_join_failure_fails_catalog_shutdown() {
        let service = ModelCatalogService::new(None);
        let result = read_session_with_shutdown(
            Arc::new(Script::panics_on_exit(vec![json!({
                "data": [],
                "nextCursor": null
            })])),
            ProviderLaunchSpec::new(vec!["scripted".into()], BTreeMap::new(), "/var/tmp".into()),
            Instant::now(),
            service.0.stop.clone(),
            service.0.cleanup_error.clone(),
        )
        .await;
        assert_eq!(result, Err(()));
        let error = service
            .shutdown()
            .await
            .expect_err("unconfirmed provider cleanup must fail catalog shutdown");
        assert!(
            error
                .to_string()
                .contains("provider task exited without a cleanup receipt")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pagination_and_page_size_are_bounded() {
        let pages = (0..MAX_CATALOG_PAGES)
            .map(|index| json!({"data":[],"nextCursor":index.to_string()}))
            .collect();
        assert_eq!(scripted(pages).await, Err(()));
        assert_eq!(
            scripted(vec![
                json!({"data":[],"extra":"x".repeat(MAX_CATALOG_PAGE_BYTES)})
            ])
            .await,
            Err(())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cache_keeps_stale_pending_values_and_notifies_only_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = notifications.clone();
        let service = ModelCatalogService::with_change_callback(
            None,
            Arc::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            }),
        );
        assert_eq!(
            service.read("account", "/home", "claude", false).await,
            CatalogRead::Unavailable
        );
        let key = ("account".to_owned(), "/home".to_owned(), "codex".to_owned());
        let value = json!({"data":[]});
        service.0.cache.lock().expect("cache").insert(
            key.clone(),
            CacheEntry {
                generation: 7,
                value: Some(value.clone()),
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        assert_eq!(
            service.read("account", "/home", "codex", false).await,
            CatalogRead::Ready(value.clone())
        );
        assert!(store_result(&service.0, &key, 7, Ok(value.clone())));
        assert!(!store_result(&service.0, &key, 7, Ok(value.clone())));
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert_eq!(
            service.read("account", "/home", "codex", false).await,
            CatalogRead::Ready(value)
        );
    }
    #[tokio::test(start_paused = true)]
    async fn cache_pruning_and_shutdown_are_bounded() {
        let service = ModelCatalogService::new(None);
        for account in 0..100 {
            service
                .0
                .cache
                .lock()
                .expect("catalog resource check")
                .insert(
                    (account.to_string(), "/var/tmp".into(), "codex".into()),
                    CacheEntry {
                        generation: 1,
                        value: Some(json!({"models":[]})),
                        expires_at: Some(Instant::now() + CATALOG_TTL),
                        error: false,
                        pending: false,
                    },
                );
        }
        service.retain_accounts(&[json!({"id":"0","home":"/var/tmp","provider":"codex"})]);
        assert_eq!(
            service
                .0
                .cache
                .lock()
                .expect("catalog resource check")
                .len(),
            1
        );
        service.shutdown().await.expect("catalog resource check");
        assert_eq!(
            service.read("new", "/var/tmp", "codex", false).await,
            CatalogRead::Unavailable
        );
    }
    #[tokio::test(start_paused = true)]
    async fn catalog_drains_notifications_while_write_receipt_waits() {
        let page = json!({"data":[],"nextCursor":null});
        assert_eq!(
            read_session(
                Arc::new(Script::new(vec![page.clone()], LANE_CAPACITY * 4)),
                ProviderLaunchSpec::new(
                    vec!["scripted".into()],
                    BTreeMap::new(),
                    "/var/tmp".into()
                ),
                Instant::now()
            )
            .await,
            Ok(page)
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn model_catalog_probe_preserves_inherited_priority() {
        use std::process::Command;
        use studio_runtime_process::provider_seam::{OpenOptions, SessionClock};
        use studio_runtime_process::provider_stdio::StdioProviderTaskFactory;
        // Catalog probes now use the temporary async stdio adapter. Keep the
        // priority assertion on that actual spawn path while the child waits.
        let mut child = StdioProviderTaskFactory {
            deadline: Some(Instant::now() + CATALOG_TIMEOUT),
            max_frame_bytes: MAX_CATALOG_PAGE_BYTES,
            max_output_bytes: Some(MAX_CATALOG_RESPONSE_BYTES),
            stderr_log: None,
        }
        .open(
            ProviderHandle::for_account("catalog-priority"),
            OpenOptions {
                clock: SessionClock::default(),
                expected_generation: Some(Generation(1)),
                launch: ProviderLaunchSpec::new(
                    vec!["/bin/sh".into(), "-c".into(), "read -r ready".into()],
                    BTreeMap::new(),
                    "/var/tmp".into(),
                ),
            },
            Shutdown::default(),
        )
        .await
        .expect("catalog caller");
        let pid = child.status.borrow().info.as_ref().expect("child info").pid;
        let nice = Command::new("ps")
            .args(["-o", "ni=", "-p", &pid.to_string()])
            .output()
            .expect("catalog priority");
        assert!(nice.status.success());
        assert_eq!(
            String::from_utf8_lossy(&nice.stdout).trim(),
            rustix::process::getpriority_process(None)
                .expect("parent nice")
                .to_string()
        );
        let io_priority = |pid: u32| {
            let output = Command::new("ionice")
                .args(["-p", &pid.to_string()])
                .output()
                .expect("I/O priority");
            assert!(output.status.success());
            output.stdout
        };
        assert_eq!(io_priority(pid), io_priority(std::process::id()));
        assert!(child.shutdown().await.result.is_ok());
    }
}
